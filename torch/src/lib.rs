//! The `torch` dialect: a high-level tensor IR modeling the graph a Torch
//! Inductor backend receives. It is deliberately small — just the operations
//! and the single ranked-tensor type needed to spell out the forward pass of a
//! GPT (see the nanoGPT demo in `tests/`). There is no lowering, no shape
//! inference and no autograd; this is IR only.
//!
//! Parameters (weights) enter the graph as `torch.get_attr` ops named after
//! their module path, mirroring `torch.fx`'s `get_attr` nodes, and the token
//! input is the function argument.

use std::any::Any;
use std::sync::Arc;

use tir::helpers::{dialect, operation};
use tir::{Context, Error, IRFormatter, Type, TypeConstraint, TypeId, parse::Span};

pub mod ops {
    pub use super::{
        AddOp, ArangeOp, CausalMaskOp, EmbeddingOp, GeluOp, GetAttrOp, LayerNormOp, LinearOp,
        MatmulOp, MulOp, ReshapeOp, SoftmaxOp, TransposeOp,
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
            MulOp,
            MatmulOp,
            LinearOp,
            LayerNormOp,
            GeluOp,
            SoftmaxOp,
            TransposeOp,
            ReshapeOp,
            CausalMaskOp,
        ],
        types: [TensorType],
    }
}

/// Element type of a [`TensorType`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DType {
    F32,
    F16,
    BF16,
    I64,
    I32,
}

impl DType {
    fn as_str(self) -> &'static str {
        match self {
            DType::F32 => "f32",
            DType::F16 => "f16",
            DType::BF16 => "bf16",
            DType::I64 => "i64",
            DType::I32 => "i32",
        }
    }

    fn from_ident(s: &str) -> Option<Self> {
        Some(match s {
            "f32" => DType::F32,
            "f16" => DType::F16,
            "bf16" => DType::BF16,
            "i64" => DType::I64,
            "i32" => DType::I32,
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

// Parameters and inputs. `get_attr` yields a named module parameter (a weight);
// `arange` materializes position ids `[0, n)`.
operation! {
    GetAttrOp {
        name: "get_attr",
        dialect: "torch",
        attributes: A {
            name: "Str",
        },
        results: R {
            result: "crate::TensorType",
        },
    }
}

operation! {
    ArangeOp {
        name: "arange",
        dialect: "torch",
        attributes: A {
            n: "Int",
        },
        results: R {
            result: "crate::TensorType",
        },
    }
}

operation! {
    EmbeddingOp {
        name: "embedding",
        dialect: "torch",
        operands: O {
            weight: "crate::TensorType",
            indices: "crate::TensorType",
        },
        results: R {
            result: "crate::TensorType",
        },
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
        results: R {
            result: "crate::TensorType",
        },
    }
}

operation! {
    MulOp {
        name: "mul",
        dialect: "torch",
        operands: O {
            lhs: "crate::TensorType",
            rhs: "crate::TensorType",
        },
        results: R {
            result: "crate::TensorType",
        },
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
        results: R {
            result: "crate::TensorType",
        },
    }
}

// A dense layer `input @ weight (+ bias)`; bias is optional.
operation! {
    LinearOp {
        name: "linear",
        dialect: "torch",
        operands: O {
            input: "crate::TensorType",
            weight: "crate::TensorType",
            bias: "?crate::TensorType",
        },
        results: R {
            result: "crate::TensorType",
        },
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
        results: R {
            result: "crate::TensorType",
        },
    }
}

operation! {
    GeluOp {
        name: "gelu",
        dialect: "torch",
        operands: O {
            input: "crate::TensorType",
        },
        results: R {
            result: "crate::TensorType",
        },
    }
}

operation! {
    SoftmaxOp {
        name: "softmax",
        dialect: "torch",
        attributes: A {
            dim: "Int",
        },
        operands: O {
            input: "crate::TensorType",
        },
        results: R {
            result: "crate::TensorType",
        },
    }
}

operation! {
    TransposeOp {
        name: "transpose",
        dialect: "torch",
        attributes: A {
            dim0: "Int",
            dim1: "Int",
        },
        operands: O {
            input: "crate::TensorType",
        },
        results: R {
            result: "crate::TensorType",
        },
    }
}

// The new shape is carried by the result type, so no shape attribute is needed.
operation! {
    ReshapeOp {
        name: "reshape",
        dialect: "torch",
        operands: O {
            input: "crate::TensorType",
        },
        results: R {
            result: "crate::TensorType",
        },
    }
}

// Applies the lower-triangular causal mask used by autoregressive attention.
operation! {
    CausalMaskOp {
        name: "causal_mask",
        dialect: "torch",
        operands: O {
            input: "crate::TensorType",
        },
        results: R {
            result: "crate::TensorType",
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tensor_type_roundtrip() {
        let context = Context::with_default_dialects();
        context.register_dialect::<TorchDialect>();

        let scalar = TensorType::new(&context, DType::F32, vec![]);
        assert_eq!(context.type_to_string(scalar), "!torch.tensor<f32>");

        let shaped = TensorType::new(&context, DType::F32, vec![1, 4, 8]);
        assert_eq!(
            context.type_to_string(shaped),
            "!torch.tensor<f32, 1, 4, 8>"
        );

        // Shape and dtype are part of identity (textual round-trip through the
        // IR parser is exercised by the nanoGPT demo in tests/).
        assert_ne!(TensorType::new(&context, DType::I64, vec![1, 4, 8]), shaped);
        assert_ne!(TensorType::new(&context, DType::F32, vec![1, 4]), shaped);
    }
}
