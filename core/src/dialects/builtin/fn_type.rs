use std::any::Any;
use std::sync::Arc;

use crate::ty::TypeConstraint;
use crate::{Context, Error, IRFormatter, Type, TypeId, parse::Span};

use crate as tir;

/// The type of a function value, written `!fn<(!i32, !i32) -> !i32>`.
///
/// A λ node produces one of these, and a call consumes it, so "which function
/// runs here" is a def-use question. A variadic signature ends in a
/// variadic-tail parameter type (see [`Type::is_variadic_tail`]) rather than
/// carrying a separate flag.
pub struct FnType {
    params: Vec<Arc<dyn Type>>,
    ret: Arc<dyn Type>,
}

impl FnType {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(context: &Context, params: &[TypeId], ret: TypeId) -> TypeId {
        let params = params
            .iter()
            .map(|param| context.get_type_data(*param))
            .collect();
        context.get_type_id(Arc::new(Self {
            params,
            ret: context.get_type_data(ret),
        }))
    }

    pub fn params(&self, context: &Context) -> Vec<TypeId> {
        self.params
            .iter()
            .map(|param| context.get_type_id(param.clone()))
            .collect()
    }

    pub fn ret(&self, context: &Context) -> TypeId {
        context.get_type_id(self.ret.clone())
    }

    /// The parameter and return types of `value`, when it is a function value.
    pub fn signature_of(context: &Context, value: crate::ValueId) -> Option<(Vec<TypeId>, TypeId)> {
        let data = context.get_type_data(context.get_value(value).ty());
        let signature = (data.as_ref() as &dyn Any).downcast_ref::<FnType>()?;
        Some((signature.params(context), signature.ret(context)))
    }

    /// Whether the signature accepts any number of further arguments.
    pub fn is_variadic(&self) -> bool {
        self.params
            .last()
            .is_some_and(|param| param.is_variadic_tail())
    }
}

impl TypeConstraint for FnType {}

impl Type for FnType {
    fn dialect(&self) -> &'static str {
        "builtin"
    }

    fn parse_key() -> &'static str {
        "fn"
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
        if !parser.parse_token("(") {
            return Err((parser.span(), Error::ExpectedToken("(")));
        }
        let mut params = vec![];
        if !parser.parse_token(")") {
            loop {
                params.push(
                    parser
                        .parse_type(context)?
                        .ok_or_else(|| (parser.span(), Error::ExpectedType))?,
                );
                if parser.parse_token(")") {
                    break;
                }
                if !parser.parse_token(",") {
                    return Err((parser.span(), Error::ExpectedToken(",")));
                }
            }
        }
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
    }

    fn print(&self, fmt: &mut IRFormatter<'_>) -> Result<(), std::fmt::Error> {
        fmt.write("fn<(")?;
        for (index, param) in self.params.iter().enumerate() {
            if index > 0 {
                fmt.write(", ")?;
            }
            print_nested(fmt, param)?;
        }
        fmt.write(") -> ")?;
        print_nested(fmt, &self.ret)?;
        fmt.write(">")
    }

    fn eq(&self, other: &dyn Type) -> bool {
        let Some(other) = (other as &dyn Any).downcast_ref::<FnType>() else {
            return false;
        };
        self.ret.eq(other.ret.as_ref())
            && self.params.len() == other.params.len()
            && self
                .params
                .iter()
                .zip(&other.params)
                .all(|(left, right)| left.eq(right.as_ref()))
    }

    fn hash(&self, state: &mut dyn std::hash::Hasher) {
        for param in &self.params {
            state.write_usize(Arc::as_ptr(param) as *const () as usize);
        }
        state.write_usize(self.params.len());
        state.write_usize(Arc::as_ptr(&self.ret) as *const () as usize);
    }
}

fn print_nested(fmt: &mut IRFormatter<'_>, ty: &Arc<dyn Type>) -> Result<(), std::fmt::Error> {
    fmt.write("!")?;
    if ty.dialect() != "builtin" {
        fmt.write(format!("{}.", ty.dialect()))?;
    }
    ty.print(fmt)
}

// Takes a function's address as data, for a language that stores function
// pointers in memory. `!fn` values never live in memory themselves.
tir::operation! {
    FnToPtrOp {
        name: "fn_to_ptr",
        dialect: "builtin",
        operands: O {
            callee: "crate::builtin::FnType",
        },
        results: R {
            result: "crate::ptr::PtrType",
        },
        interfaces: [crate::Pure],
    }
}

impl crate::Pure for FnToPtrOp {}

// Recovers a callable function value from an address, asserting the signature
// the call site expects. The inverse of `fn_to_ptr`.
tir::operation! {
    PtrToFnOp {
        name: "ptr_to_fn",
        dialect: "builtin",
        operands: O {
            address: "crate::ptr::PtrType",
        },
        results: R {
            result: "crate::builtin::FnType",
        },
        interfaces: [crate::Pure],
    }
}

impl crate::Pure for PtrToFnOp {}
