//! The `vector` dialect: a small, target-independent vocabulary for SIMD-style
//! arithmetic. A [`VectorType`] is either statically sized (`vec<8xi32>`) or
//! dynamically sized (`vec<i32>`); a dynamic vector's elementwise operation takes
//! a vector-length (`vl`) operand bounding the active lanes, while a static one
//! reads its lane count straight from the type.

use tir_symbolic::term::{no_carrier, valid};

use crate::ty::{Ctor, Kind, Param, Terms, TypeArg, TypeCtor, TypeSpelling};
use crate::{Context, Error, TypeId, dialect};

use crate as tir;

pub mod ops {
    pub use super::{AddOp, MulOp, SubOp, VectorLenOp, add, mul, sub, vector_len};
}

dialect! {
    VectorDialect {
        name: "vector",
        operations: [
            AddOp,
            SubOp,
            MulOp,
            VectorLenOp,
        ],
        types: [VectorType],
    }
}

// `vector.v<N, T>`: `N` lanes of `T`.
static VEC: Ctor = Ctor::new(TypeCtor {
    dialect: "vector",
    name: "v",
    params: &[
        ("length", Param::One(Kind::Nat)),
        ("element", Param::One(Kind::Data)),
    ],
    kind: Kind::Data,
    validate: valid,
    carrier: no_carrier,
});

// The lane count of a dynamic vector: a number the target picks at run time,
// as RVV's VLMAX or SVE's vector length. A dynamic vector is `vector.v<vl, T>`.
static VL: Ctor = Ctor::new(TypeCtor {
    dialect: "vector",
    name: "vl",
    params: &[],
    kind: Kind::Nat,
    validate: valid,
    carrier: no_carrier,
});

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
        TypeSpelling {
            ctor: VEC.id(context),
            keys: &["vec"],
            parse: |_, parser, context| {
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
                Ok(Self::new(context, element, length))
            },
            print: |spellings, ty, fmt| {
                let vector = Self::view(spellings, ty).expect("a vector");
                fmt.write("vector.vec<")?;
                if let Some(length) = vector.length {
                    fmt.write(format!("{length}x"))?;
                }
                spellings.print_body(vector.element, fmt)?;
                fmt.write(">")
            },
            variadic_tail: false,
        }
    }
}

macro_rules! vector_binop {
    ($op:ident, $definition:ident) => {
        $definition!();
    };
}

// An elementwise operation works lane by lane: its operands and its result
// are one vector type.

vector_binop!(AddOp, __cdsl_vector_AddOp);
vector_binop!(SubOp, __cdsl_vector_SubOp);
vector_binop!(MulOp, __cdsl_vector_MulOp);

// `vector_len(avl)` yields the number of lanes the target grants for a request
// of `avl` — `min(avl, VLMAX)` on RVV, where it selects as `vsetvli rd, avl`.
// The strip-mine primitive: a loop asks for the remaining trip count and
// advances by however many lanes the hardware grants, feeding the result to its
// elementwise ops' `vl` operands (EVL-style tail folding, no epilogue loop).
// The `sew` attribute names the element width the grant is for — how many
// lanes fit is a function of it (VLMAX = VLEN / SEW · LMUL).
__cdsl_vector_VectorLenOp!();
