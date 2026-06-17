//! The `torch` dialect: a high-level tensor IR that mirrors, one-to-one, the
//! nodes of a `torch.compile` (TorchDynamo) FX graph. The companion Python
//! backend in `python/tir_inductor/` registers itself with `torch.compile`,
//! receives the FX graph of a model's forward pass, and lowers each node into
//! one of these ops. This crate is IR only — there is no execution, no shape
//! inference and no lowering past this level.
//!
//! Parameters and buffers enter the graph as `torch.get_attr` ops named after
//! their module path (like `torch.fx`'s `get_attr` nodes); the model's tensor
//! inputs are the function arguments. Python scalar arguments that ride along
//! an op (a scale factor, a fill value) are carried as string attributes so the
//! exact literal (`-inf`, `0.5`) survives a textual round-trip.

use std::any::Any;
use std::sync::Arc;

use tir::helpers::{dialect, operation};
use tir::{Context, Error, IRFormatter, Type, TypeConstraint, TypeId, parse::Span};

pub mod ops {
    pub use super::{
        AddOp, ArangeOp, ContiguousOp, EmbeddingOp, EqOp, GeluOp, GetAttrOp, LayerNormOp, LinearOp,
        MaskedFillOp, MatmulOp, MulScalarOp, SliceOp, SoftmaxOp, SplitOp, TransposeOp, ViewOp,
    };
}

dialect! {
    TorchDialect {
        name: "torch",
        operations: [
            GetAttrOp,
            ArangeOp,
            EmbeddingOp,
            AddOp,
            MatmulOp,
            MulScalarOp,
            LinearOp,
            LayerNormOp,
            GeluOp,
            SoftmaxOp,
            TransposeOp,
            ViewOp,
            SplitOp,
            SliceOp,
            EqOp,
            MaskedFillOp,
            ContiguousOp,
        ],
        types: [TensorType],
    }
}

/// Element type of a [`TensorType`], matching the `torch.dtype`s that appear in
/// the demo graph.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DType {
    F32,
    F16,
    BF16,
    F64,
    I64,
    I32,
    /// Boolean tensor (`torch.bool`), printed `i1`.
    Bool,
}

impl DType {
    fn as_str(self) -> &'static str {
        match self {
            DType::F32 => "f32",
            DType::F16 => "f16",
            DType::BF16 => "bf16",
            DType::F64 => "f64",
            DType::I64 => "i64",
            DType::I32 => "i32",
            DType::Bool => "i1",
        }
    }

    fn from_ident(s: &str) -> Option<Self> {
        Some(match s {
            "f32" => DType::F32,
            "f16" => DType::F16,
            "bf16" => DType::BF16,
            "f64" => DType::F64,
            "i64" => DType::I64,
            "i32" => DType::I32,
            "i1" => DType::Bool,
            _ => return None,
        })
    }

    // Stable integer codes shared with the Python binding (`tir_inductor`), which
    // builds tensor types structurally and so cannot pass a dtype string.
    fn from_code(code: u32) -> Option<Self> {
        Some(match code {
            0 => DType::F32,
            1 => DType::F16,
            2 => DType::BF16,
            3 => DType::F64,
            4 => DType::I64,
            5 => DType::I32,
            6 => DType::Bool,
            _ => return None,
        })
    }
}

/// A ranked tensor, written `!torch.tensor<f32>` (a scalar) or
/// `!torch.tensor<f32, 1, 4, 8>` (element type followed by static dimensions).
/// Shapes are descriptive only — operand constraints check that a value is some
/// tensor, not that its shape matches.
pub struct TensorType {
    dtype: DType,
    shape: Vec<i64>,
}

impl TensorType {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(context: &Context, dtype: DType, shape: Vec<i64>) -> TypeId {
        context.get_type_id(Arc::new(Self { dtype, shape }))
    }

    pub fn dtype(&self) -> DType {
        self.dtype
    }

    pub fn shape(&self) -> &[i64] {
        &self.shape
    }
}

// `TensorType` has a variable-length shape, which `#[derive(TirType)]` cannot
// express, so its structural builder is registered by hand. Arguments are the
// dtype code (a `u32`) followed by one `i64` per dimension. This is what the
// Python binding calls to construct result types.
#[tir::linkme::distributed_slice(tir::TYPE_SCHEMAS)]
#[linkme(crate = tir::linkme)]
static TENSOR_TYPE_SCHEMA: tir::TypeSchema = tir::TypeSchema {
    dialect: "torch",
    name: "tensor",
    params: &[],
    build: |context, args| {
        let Some(tir::TypeArg::U32(code)) = args.first() else {
            return Err("type 'torch.tensor' expects a dtype code (u32) first".to_string());
        };
        let dtype = DType::from_code(*code).ok_or_else(|| format!("unknown dtype code {code}"))?;
        let mut shape = Vec::with_capacity(args.len() - 1);
        for arg in &args[1..] {
            match arg {
                tir::TypeArg::I64(dim) => shape.push(*dim),
                _ => return Err("type 'torch.tensor' dimensions must be i64".to_string()),
            }
        }
        Ok(TensorType::new(context, dtype, shape))
    },
};

impl TypeConstraint for TensorType {}

impl Type for TensorType {
    fn dialect(&self) -> &'static str {
        "torch"
    }

    fn parse_key() -> &'static str {
        "tensor"
    }

    fn parse<'src>(
        _mnemonic: &str,
        parser: &mut tir::parse::text::Parser<'src>,
        context: &Context,
    ) -> Result<TypeId, (Span, Error)> {
        use tir::parse::common::Cursor;
        if !parser.parse_token("<") {
            return Err((parser.span(), Error::ExpectedToken("<")));
        }
        let dtype = parser
            .parse_ident()
            .and_then(DType::from_ident)
            .ok_or_else(|| (parser.span(), Error::ExpectedType))?;
        let mut shape = Vec::new();
        while parser.parse_token(",") {
            let dim = parser
                .parse_number()
                .ok_or_else(|| (parser.span(), Error::ExpectedType))?;
            shape.push(dim);
        }
        if !parser.parse_token(">") {
            return Err((parser.span(), Error::ExpectedToken(">")));
        }
        Ok(Self::new(context, dtype, shape))
    }

    fn print(&self, fmt: &mut IRFormatter<'_>) -> Result<(), std::fmt::Error> {
        fmt.write("tensor<")?;
        fmt.write(self.dtype.as_str())?;
        for dim in &self.shape {
            fmt.write(format!(", {dim}"))?;
        }
        fmt.write(">")
    }

    fn eq(&self, other: &dyn Type) -> bool {
        (other as &dyn Any)
            .downcast_ref::<TensorType>()
            .map(|o| o.dtype == self.dtype && o.shape == self.shape)
            .unwrap_or(false)
    }
}

// A named module parameter or buffer (an FX `get_attr` / lifted placeholder).
operation! {
    GetAttrOp {
        name: "get_attr",
        dialect: "torch",
        attributes: A { name: "Str" },
        results: R { result: "crate::TensorType" },
    }
}

// `torch.arange(0, n)` — produces position ids.
operation! {
    ArangeOp {
        name: "arange",
        dialect: "torch",
        attributes: A { n: "Int" },
        results: R { result: "crate::TensorType" },
    }
}

// `embedding(weight, indices)`.
operation! {
    EmbeddingOp {
        name: "embedding",
        dialect: "torch",
        operands: O {
            weight: "crate::TensorType",
            indices: "crate::TensorType",
        },
        results: R { result: "crate::TensorType" },
    }
}

operation! {
    AddOp {
        name: "add",
        dialect: "torch",
        operands: O {
            lhs: "crate::TensorType",
            rhs: "crate::TensorType",
        },
        results: R { result: "crate::TensorType" },
    }
}

operation! {
    MatmulOp {
        name: "matmul",
        dialect: "torch",
        operands: O {
            lhs: "crate::TensorType",
            rhs: "crate::TensorType",
        },
        results: R { result: "crate::TensorType" },
    }
}

// Multiply a tensor by a Python scalar (e.g. the attention scale `* 0.5`); the
// literal is kept verbatim as a string.
operation! {
    MulScalarOp {
        name: "mul_scalar",
        dialect: "torch",
        attributes: A { value: "Str" },
        operands: O { input: "crate::TensorType" },
        results: R { result: "crate::TensorType" },
    }
}

// A dense layer `input @ weight.T`. A bias, when present, is lowered to a
// separate `torch.add`, so this op has a fixed two-operand shape.
operation! {
    LinearOp {
        name: "linear",
        dialect: "torch",
        operands: O {
            input: "crate::TensorType",
            weight: "crate::TensorType",
        },
        results: R { result: "crate::TensorType" },
    }
}

operation! {
    LayerNormOp {
        name: "layer_norm",
        dialect: "torch",
        operands: O {
            input: "crate::TensorType",
            weight: "crate::TensorType",
            bias: "crate::TensorType",
        },
        results: R { result: "crate::TensorType" },
    }
}

operation! {
    GeluOp {
        name: "gelu",
        dialect: "torch",
        operands: O { input: "crate::TensorType" },
        results: R { result: "crate::TensorType" },
    }
}

operation! {
    SoftmaxOp {
        name: "softmax",
        dialect: "torch",
        attributes: A { dim: "Int" },
        operands: O { input: "crate::TensorType" },
        results: R { result: "crate::TensorType" },
    }
}

operation! {
    TransposeOp {
        name: "transpose",
        dialect: "torch",
        attributes: A { dim0: "Int", dim1: "Int" },
        operands: O { input: "crate::TensorType" },
        results: R { result: "crate::TensorType" },
    }
}

// `tensor.view(...)` — the new shape is carried by the result type.
operation! {
    ViewOp {
        name: "view",
        dialect: "torch",
        operands: O { input: "crate::TensorType" },
        results: R { result: "crate::TensorType" },
    }
}

// One chunk of `tensor.split(size, dim)`: `index` selects which chunk.
operation! {
    SplitOp {
        name: "split",
        dialect: "torch",
        attributes: A { dim: "Int", size: "Int", index: "Int" },
        operands: O { input: "crate::TensorType" },
        results: R { result: "crate::TensorType" },
    }
}

// A slice `tensor[spec]`; `spec` is the Python index expression as text.
operation! {
    SliceOp {
        name: "slice",
        dialect: "torch",
        attributes: A { spec: "Str" },
        operands: O { input: "crate::TensorType" },
        results: R { result: "crate::TensorType" },
    }
}

// Elementwise `tensor == value` against a Python scalar, yielding a bool tensor.
operation! {
    EqOp {
        name: "eq",
        dialect: "torch",
        attributes: A { value: "Str" },
        operands: O { input: "crate::TensorType" },
        results: R { result: "crate::TensorType" },
    }
}

// `input.masked_fill(mask, value)`.
operation! {
    MaskedFillOp {
        name: "masked_fill",
        dialect: "torch",
        attributes: A { value: "Str" },
        operands: O {
            input: "crate::TensorType",
            mask: "crate::TensorType",
        },
        results: R { result: "crate::TensorType" },
    }
}

// `tensor.contiguous()` — a layout hint, kept as an explicit node.
operation! {
    ContiguousOp {
        name: "contiguous",
        dialect: "torch",
        operands: O { input: "crate::TensorType" },
        results: R { result: "crate::TensorType" },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tir::Operation;
    use tir::builtin::FuncOp;
    use tir::parse::ir::parse_ir;

    #[test]
    fn tensor_type_print() {
        let context = Context::with_default_dialects();
        context.register_dialect::<TorchDialect>();

        let scalar = TensorType::new(&context, DType::F32, vec![]);
        assert_eq!(context.type_to_string(scalar), "!torch.tensor<f32>");
        let shaped = TensorType::new(&context, DType::F32, vec![1, 4, 8]);
        assert_eq!(
            context.type_to_string(shaped),
            "!torch.tensor<f32, 1, 4, 8>"
        );
        let mask = TensorType::new(&context, DType::Bool, vec![1, 1, 4, 4]);
        assert_eq!(
            context.type_to_string(mask),
            "!torch.tensor<i1, 1, 1, 4, 4>"
        );

        assert_ne!(TensorType::new(&context, DType::I64, vec![1, 4, 8]), shaped);
        assert_ne!(TensorType::new(&context, DType::F32, vec![1, 4]), shaped);
    }

    /// A slice of the lowered IR must survive a textual round-trip, exercising
    /// parse and print for the ops, the tensor type and the string/int attrs.
    #[test]
    fn op_text_roundtrip() {
        let context = Context::with_default_dialects();
        context.register_dialect::<TorchDialect>();

        let src = "func @f(%idx: !torch.tensor<i64, 1, 4>) -> !torch.tensor<f32, 1, 2, 4, 4> {\n  \
            %w = torch.get_attr {name = \"attn.c_attn.weight\"} : !torch.tensor<f32, 24, 8>\n  \
            %e = torch.embedding %w, %idx : !torch.tensor<f32, 1, 4, 8>\n  \
            %q = torch.split %e {dim = 2, size = 8, index = 0} : !torch.tensor<f32, 1, 4, 8>\n  \
            %a = torch.mul_scalar %q {value = \"0.5\"} : !torch.tensor<f32, 1, 2, 4, 4>\n  \
            %s = torch.softmax %a {dim = 3} : !torch.tensor<f32, 1, 2, 4, 4>\n  \
            return %s\n\
            }";

        let func = parse_ir::<FuncOp>(&context, src).expect("parse");
        assert!(func.verify(&context).is_ok());

        let ctx2 = Context::with_default_dialects();
        ctx2.register_dialect::<TorchDialect>();
        let reparsed = parse_ir::<FuncOp>(&ctx2, src).expect("re-parse");
        let mut a = String::new();
        let mut b = String::new();
        func.print(&mut IRFormatter::new(&mut a)).unwrap();
        reparsed.print(&mut IRFormatter::new(&mut b)).unwrap();
        assert_eq!(a, b);
    }
}
