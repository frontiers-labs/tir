//! The `vector` dialect: a small, target-independent vocabulary for SIMD-style
//! arithmetic. A [`VectorType`] is either statically sized (`vec<8xi32>`) or
//! dynamically sized (`vec<i32>`); a dynamic vector's elementwise operation takes
//! a vector-length (`vl`) operand bounding the active lanes, while a static one
//! reads its lane count straight from the type.

use tir_symbolic::term::{no_carrier, valid};

use crate::ty::{Ctor, Terms, TypeArg, TypeSpelling};
use crate::{Context, Error, TypeId, dialect};

use crate as tir;

pub mod ops {
    pub use super::{ADD, MUL, SUB, VECTOR_LEN, add, mul, sub, vector_len};
}

dialect! {
    VectorDialect {
        name: "vector",
        operations: [
            ADD,
            SUB,
            MUL,
            VECTOR_LEN,
        ],
        types: [VectorType],
    }
}

// `vector.v<N, T>`: `N` lanes of `T`.
static VEC: Ctor = Ctor::new(
    ::tir::helpers::type_ctor! { file: "defs/types/core_src_dialects_vector_mod.cdsl", binding: constructors::VEC, hosts: { VEC_validate => valid, VEC_carrier => no_carrier } },
);

// The lane count of a dynamic vector: a number the target picks at run time,
// as RVV's VLMAX or SVE's vector length. A dynamic vector is `vector.v<vl, T>`.
static VL: Ctor = Ctor::new(
    ::tir::helpers::type_ctor! { file: "defs/types/core_src_dialects_vector_mod.cdsl", binding: constructors::VL, hosts: { VL_validate => valid, VL_carrier => no_carrier } },
);

/// A vector type, written `vec<8xi32>` (static) or `vec<i32>` (dynamic). A static
/// vector fixes its lane count; a dynamic one leaves it unknown until run time:
/// its length is the term `vector.vl`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VectorType {
    element: TypeId,
    length: Option<u32>,
}

impl VectorType {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(context: &Context, element: TypeId, length: Option<u32>) -> TypeId {
        let terms = context.terms();
        let length = match length {
            Some(length) => TypeArg::Nat(u64::from(length)),
            None => TypeArg::Type(terms.intern(VL.id(terms), vec![]).expect("the lane count")),
        };
        terms
            .intern(VEC.id(terms), vec![length, TypeArg::Type(element)])
            .expect("a vector of a data type")
    }

    /// A statically sized vector of `length` lanes.
    pub fn fixed(context: &Context, element: TypeId, length: u32) -> TypeId {
        Self::new(context, element, Some(length))
    }

    /// A dynamically sized vector whose lane count is unknown at compile time.
    pub fn dynamic(context: &Context, element: TypeId) -> TypeId {
        Self::new(context, element, None)
    }

    pub fn view(terms: &impl AsRef<Terms>, ty: TypeId) -> Option<Self> {
        let terms = terms.as_ref();
        if !VEC.is(terms, ty) {
            return None;
        }
        Some(Self {
            element: terms.child(ty, 1)?,
            length: terms
                .nat(ty, 0)
                .map(|length| u32::try_from(length).expect("a 32-bit lane count")),
        })
    }

    pub fn element(&self) -> TypeId {
        self.element
    }

    pub fn length(&self) -> Option<u32> {
        self.length
    }

    pub fn spelling(context: &Context) -> TypeSpelling {
        {
            const __TIR_TYPE_PARSE: ::tir::TypeParser = |_, parser, context| {
                use tir::parse::common::Cursor;
                if !parser.parse_token("<") {
                    return Err((parser.span(), Error::ExpectedToken("<")));
                }
                // A leading `N x` gives a static length; its absence means dynamic.
                let length = if let Some(n) = parser.parse_number() {
                    if n < 0 || !parser.parse_token("x") {
                        return Err((parser.span(), Error::ExpectedToken("x")));
                    }
                    Some(n as u32)
                } else {
                    None
                };
                let name = parser
                    .parse_ident()
                    .ok_or_else(|| (parser.span(), Error::ExpectedType))?;
                let element = context
                    .parse_type_mnemonic("builtin", name)
                    .map_err(|err| (parser.span(), err))?;
                if !parser.parse_token(">") {
                    return Err((parser.span(), Error::ExpectedToken(">")));
                }
                Ok(VectorType::new(context, element, length))
            };
            const __TIR_TYPE_PRINT: ::tir::TypePrinter = |spellings, ty, fmt| {
                let vector = VectorType::view(spellings, ty).expect("a vector");
                fmt.write("vector.vec<")?;
                if let Some(length) = vector.length {
                    fmt.write(format!("{length}x"))?;
                }
                spellings.print_body(vector.element, fmt)?;
                fmt.write(">")
            };
            const __TIR_TYPE_CTOR_ID: fn(&::tir::Context) -> ::tir::CtorId =
                |context| VEC.id(context);
            ::tir::helpers::type_spelling! { file: "defs/types/core_src_dialects_vector_mod.cdsl", binding: spellings::VectorType, context: context, hosts: { VectorType_parse => __TIR_TYPE_PARSE, VectorType_print => __TIR_TYPE_PRINT, VectorType_ctor_id => __TIR_TYPE_CTOR_ID } }
        }
    }
}

// An elementwise operation works lane by lane: its operands and its result
// are one vector type.
fn same_vector(context: &Context, op: &tir::OpHandle) -> Result<(), Error> {
    let ty = |value: &tir::ValueId| context.get_value(*value).ty();
    let result = ty(&op.results()[0]);
    if op
        .operands()
        .iter()
        .take(2)
        .all(|value| ty(value) == result)
    {
        return Ok(());
    }
    Err(Error::VerificationError(format!(
        "vector.{} operands must have the result's vector type",
        op.name()
    )))
}

tir::helpers::op_def! {
    file: "defs/converted/dialects_vector_mod.cdsl",
    hosts: {
        add_exec_model => |operation, graph| {
            const RECIPE: tir::sem::SemRecipe = tir::sem::SemRecipe {
                steps: &[
                    tir::sem::SemRecipeStep::Operand(0),
                    tir::sem::SemRecipeStep::Splice("get_vlen"),
                    tir::sem::SemRecipeStep::Splice("get_sew"),
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::Split,
                        arguments: &[0, 1, 2],
                    },
                    tir::sem::SemRecipeStep::Operand(1),
                    tir::sem::SemRecipeStep::Splice("get_vlen"),
                    tir::sem::SemRecipeStep::Splice("get_sew"),
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::Split,
                        arguments: &[4, 5, 6],
                    },
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::Zip,
                        arguments: &[3, 7],
                    },
                    tir::sem::SemRecipeStep::Argument(0),
                    tir::sem::SemRecipeStep::Argument(1),
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::Add,
                        arguments: &[9, 10],
                    },
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::Map,
                        arguments: &[8, 11],
                    },
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::IterConcat,
                        arguments: &[12],
                    },
                ],
                root: 13,
            };
            tir::sem::build_recipe_with(
                operation,
                graph,
                &RECIPE,
                |operation, name, graph| match name {
                    "get_vlen" => (vlen_node)(operation, graph).into(),
                    "get_sew" => (sew_node)(operation, graph).into(),
                    _ => None,
                },
            )
        },
        add_verifier => vector_binop_verifiable_verify_impl,
        sub_exec_model => |operation, graph| {
            const RECIPE: tir::sem::SemRecipe = tir::sem::SemRecipe {
                steps: &[
                    tir::sem::SemRecipeStep::Operand(0),
                    tir::sem::SemRecipeStep::Splice("get_vlen"),
                    tir::sem::SemRecipeStep::Splice("get_sew"),
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::Split,
                        arguments: &[0, 1, 2],
                    },
                    tir::sem::SemRecipeStep::Operand(1),
                    tir::sem::SemRecipeStep::Splice("get_vlen"),
                    tir::sem::SemRecipeStep::Splice("get_sew"),
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::Split,
                        arguments: &[4, 5, 6],
                    },
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::Zip,
                        arguments: &[3, 7],
                    },
                    tir::sem::SemRecipeStep::Argument(0),
                    tir::sem::SemRecipeStep::Argument(1),
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::Sub,
                        arguments: &[9, 10],
                    },
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::Map,
                        arguments: &[8, 11],
                    },
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::IterConcat,
                        arguments: &[12],
                    },
                ],
                root: 13,
            };
            tir::sem::build_recipe_with(
                operation,
                graph,
                &RECIPE,
                |operation, name, graph| match name {
                    "get_vlen" => (vlen_node)(operation, graph).into(),
                    "get_sew" => (sew_node)(operation, graph).into(),
                    _ => None,
                },
            )
        },
        sub_verifier => vector_binop_verifiable_verify_impl,
        mul_exec_model => |operation, graph| {
            const RECIPE: tir::sem::SemRecipe = tir::sem::SemRecipe {
                steps: &[
                    tir::sem::SemRecipeStep::Operand(0),
                    tir::sem::SemRecipeStep::Splice("get_vlen"),
                    tir::sem::SemRecipeStep::Splice("get_sew"),
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::Split,
                        arguments: &[0, 1, 2],
                    },
                    tir::sem::SemRecipeStep::Operand(1),
                    tir::sem::SemRecipeStep::Splice("get_vlen"),
                    tir::sem::SemRecipeStep::Splice("get_sew"),
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::Split,
                        arguments: &[4, 5, 6],
                    },
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::Zip,
                        arguments: &[3, 7],
                    },
                    tir::sem::SemRecipeStep::Argument(0),
                    tir::sem::SemRecipeStep::Argument(1),
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::Mul,
                        arguments: &[9, 10],
                    },
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::Map,
                        arguments: &[8, 11],
                    },
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::IterConcat,
                        arguments: &[12],
                    },
                ],
                root: 13,
            };
            tir::sem::build_recipe_with(
                operation,
                graph,
                &RECIPE,
                |operation, name, graph| match name {
                    "get_vlen" => (vlen_node)(operation, graph).into(),
                    "get_sew" => (sew_node)(operation, graph).into(),
                    _ => None,
                },
            )
        },
        mul_verifier => vector_binop_verifiable_verify_impl,
    },
}

// `vector_len(avl)` yields the number of lanes the target grants for a request
// of `avl` — `min(avl, VLMAX)` on RVV, where it selects as `vsetvli rd, avl`.
// The strip-mine primitive: a loop asks for the remaining trip count and
// advances by however many lanes the hardware grants, feeding the result to its
// elementwise ops' `vl` operands (EVL-style tail folding, no epilogue loop).
// The `sew` attribute names the element width the grant is for — how many
// lanes fit is a function of it (VLMAX = VLEN / SEW · LMUL).

fn vlen_node(op: &tir::OpHandle, g: &mut tir::sem::SemGraph) -> tir::NodeId {
    let context = op.context.clone();
    let ty = context.get_value(op.results()[0]).ty();
    let length = VectorType::view(&context, ty).and_then(|t| t.length());
    let Some(length) = length.filter(|_| op.operands().len() <= 2) else {
        let n = g.add_node(tir::sem::SymKind::Symbol);
        g.set_leaf_data(n, tir::sem::SymPayload::SymbolId(2));
        return n;
    };
    let n = g.add_node(tir::sem::SymKind::Constant);
    g.set_leaf_data(
        n,
        tir::sem::SymPayload::Int(tir_adt::APInt::new(32, u64::from(length))),
    );
    n
}

fn sew_node(op: &tir::OpHandle, g: &mut tir::sem::SemGraph) -> tir::NodeId {
    let context = op.context.clone();
    let ty = context.get_value(op.results()[0]).ty();
    let width = VectorType::view(&context, ty)
        .and_then(|t| crate::builtin::IntegerType::view(&context, t.element()))
        .map_or(0, |i| i.width());
    let n = g.add_node(tir::sem::SymKind::Constant);
    g.set_leaf_data(n, tir::sem::SymPayload::Int(tir_adt::APInt::new(32, width)));
    n
}

fn vector_binop_verifiable_verify_impl(
    operation: &tir::OpHandle,
    context: &Context,
) -> Result<(), Error> {
    same_vector(context, operation)
}
