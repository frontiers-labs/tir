use tir_symbolic::term::{no_carrier, valid};

use crate::parse::common::Cursor;
use crate::ty::{Ctor, Kind, Param, Terms, TypeArg, TypeCtor, TypeSpelling};
use crate::{Context, Error, TypeId};

use crate as tir;

static VARARGS: Ctor = Ctor::new(TypeCtor {
    dialect: "builtin",
    name: "varargs",
    params: &[],
    kind: Kind::Data,
    validate: valid,
    carrier: no_carrier,
});

/// The open tail of a function's parameter list, independent of its source language.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VarArgsType;

impl VarArgsType {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(context: &Context) -> TypeId {
        let terms = context.terms();
        terms
            .intern(VARARGS.id(terms), vec![])
            .expect("the varargs type")
    }

    pub fn view(terms: &impl AsRef<Terms>, ty: TypeId) -> Option<Self> {
        let terms = terms.as_ref();
        VARARGS.is(terms, ty).then_some(Self)
    }

    pub fn spelling(context: &Context) -> TypeSpelling {
        TypeSpelling {
            ctor: VARARGS.id(context),
            keys: &["varargs"],
            parse: |_, _, context| Ok(Self::new(context)),
            print: |_, _, fmt| fmt.write("varargs"),
            variadic_tail: true,
        }
    }
}

static FN: Ctor = Ctor::new(TypeCtor {
    dialect: "builtin",
    name: "fn",
    params: &[
        ("params", Param::List(Kind::Data)),
        ("ret", Param::One(Kind::Data)),
    ],
    kind: Kind::Data,
    validate: valid,
    carrier: no_carrier,
});

/// The type of a function value, written `!fn<(!i32, !i32) -> !i32>`.
///
/// A λ node produces one of these, and a call consumes it, so "which function
/// runs here" is a def-use question. A variadic signature ends in a
/// variadic-tail parameter type (see [`Context::is_variadic_tail`]) rather
/// than carrying a separate flag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FnType {
    params: Vec<TypeId>,
    ret: TypeId,
}

impl FnType {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(context: &Context, params: &[TypeId], ret: TypeId) -> TypeId {
        let terms = context.terms();
        let params = params.iter().copied().map(TypeArg::Type).collect();
        terms
            .intern(
                FN.id(terms),
                vec![TypeArg::List(params), TypeArg::Type(ret)],
            )
            .expect("a signature of data types")
    }

    pub fn view(terms: &impl AsRef<Terms>, ty: TypeId) -> Option<Self> {
        let terms = terms.as_ref();
        FN.is(terms, ty).then(|| Self {
            params: super::tuple::type_list(terms.arg(ty, 0)),
            ret: terms.child(ty, 1).expect("a return type"),
        })
    }

    pub fn params(&self) -> Vec<TypeId> {
        self.params.clone()
    }

    pub fn ret(&self) -> TypeId {
        self.ret
    }

    /// The parameter and return types of `value`, when it is a function value.
    pub fn signature_of(context: &Context, value: crate::ValueId) -> Option<(Vec<TypeId>, TypeId)> {
        let signature = Self::view(context, context.get_value(value).ty())?;
        Some((signature.params, signature.ret))
    }

    pub fn spelling(context: &Context) -> TypeSpelling {
        TypeSpelling {
            ctor: FN.id(context),
            keys: &["fn"],
            parse: |_, parser, context| {
                if !parser.parse_token("<") {
                    return Err((parser.span(), Error::ExpectedToken("<")));
                }
                let params = parser
                    .parse_delimited("(", ")", |parser| {
                        parser
                            .parse_type(context)?
                            .ok_or_else(|| (parser.span(), Error::ExpectedType))
                    })?
                    .ok_or_else(|| (parser.span(), Error::ExpectedToken("(")))?;
                if !parser.parse_token("->") {
                    return Err((parser.span(), Error::ExpectedToken("->")));
                }
                let ret = parser
                    .parse_type(context)?
                    .ok_or_else(|| (parser.span(), Error::ExpectedType))?;
                if !parser.parse_token(">") {
                    return Err((parser.span(), Error::ExpectedToken(">")));
                }
                Ok(Self::new(context, &params, ret))
            },
            print: |spellings, ty, fmt| {
                let signature = Self::view(spellings, ty).expect("a function type");
                fmt.write("fn<(")?;
                for (index, param) in signature.params().into_iter().enumerate() {
                    if index > 0 {
                        fmt.write(", ")?;
                    }
                    spellings.print(param, fmt)?;
                }
                fmt.write(") -> ")?;
                spellings.print(signature.ret(), fmt)?;
                fmt.write(">")
            },
            variadic_tail: false,
        }
    }
}

// Takes a function's address as data, for a language that stores function
// pointers in memory. `!fn` values never live in memory themselves.
tir::operation! {
    FnToPtrOp {
        name: "fn_to_ptr",
        dialect: "builtin",
        scheme: "(fn<_, _>) -> ptr.p<_>",
        operands: O { callee },
        results: R { result },
        interfaces: [crate::Pure, crate::Speculatable],
    }
}

impl crate::Speculatable for FnToPtrOp {}

impl crate::Pure for FnToPtrOp {}

// Recovers a callable function value from an address, asserting the signature
// the call site expects. The inverse of `fn_to_ptr`.
tir::operation! {
    PtrToFnOp {
        name: "ptr_to_fn",
        dialect: "builtin",
        scheme: "(ptr.p<_>) -> fn<_, _>",
        operands: O { address },
        results: R { result },
        interfaces: [crate::Pure, crate::Speculatable],
    }
}

impl crate::Speculatable for PtrToFnOp {}

impl crate::Pure for PtrToFnOp {}
