use tir_symbolic::term::ctor;

use crate::parse::common::Cursor;
use crate::ty::{Terms, TypeArg, TypeSpelling};
use crate::{Context, Error, TypeId};

use crate as tir;

/// `builtin.tuple<[T]>`, written `!tuple<!a, !b>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TupleType(Vec<TypeId>);

impl TupleType {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(context: &Context, elements: Vec<TypeId>) -> TypeId {
        let elements = elements.into_iter().map(TypeArg::Type).collect();
        context
            .terms()
            .intern(ctor::TUPLE, vec![TypeArg::List(elements)])
            .expect("a tuple of data types")
    }

    pub fn view(terms: &impl AsRef<Terms>, ty: TypeId) -> Option<Self> {
        let terms = terms.as_ref();
        (terms.ctor(ty) == ctor::TUPLE).then(|| Self(type_list(terms.arg(ty, 0))))
    }

    pub fn elements(&self) -> Vec<TypeId> {
        self.0.clone()
    }

    pub fn spelling(_context: &Context) -> TypeSpelling {
        {
            const __TIR_TYPE_PARSE: ::tir::TypeParser = |_, parser, context| {
                let elements = parser
                    .parse_delimited("<", ">", |parser| {
                        parser
                            .parse_type(context)?
                            .ok_or_else(|| (parser.span(), Error::ExpectedType))
                    })?
                    .ok_or_else(|| (parser.span(), Error::ExpectedToken("<")))?;
                Ok(TupleType::new(context, elements))
            };
            const __TIR_TYPE_PRINT: ::tir::TypePrinter = |spellings, ty, fmt| {
                fmt.write("tuple<")?;
                let tuple = TupleType::view(spellings, ty).expect("a tuple type");
                for (index, element) in tuple.0.into_iter().enumerate() {
                    if index > 0 {
                        fmt.write(", ")?;
                    }
                    spellings.print(element, fmt)?;
                }
                fmt.write(">")
            };
            const __TIR_TYPE_CTOR_ID: fn(&::tir::Context) -> ::tir::CtorId = |_context| ctor::TUPLE;
            ::tir::helpers::type_spelling! { file: "defs/types/core_src_dialects_builtin_tuple.cdsl", binding: spellings::TupleType, context: _context, hosts: { TupleType_parse => __TIR_TYPE_PARSE, TupleType_print => __TIR_TYPE_PRINT, TupleType_ctor_id => __TIR_TYPE_CTOR_ID } }
        }
    }
}

pub(crate) fn type_list(arg: Option<TypeArg>) -> Vec<TypeId> {
    let Some(TypeArg::List(items)) = arg else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| match item {
            TypeArg::Type(ty) => Some(*ty),
            _ => None,
        })
        .collect()
}

tir::helpers::op_def! {
    file: "defs/converted/dialects_builtin_tuple.cdsl",
    hosts: {
        make_tuple_verifier => crate::builtin::make_tuple_verify_impl,
        make_tuple_iface_Interp => (crate :: interp :: Interp, &crate::interp::__TIR_IFACE_MakeTupleOp_Interp),
        make_tuple_iface_Speculatable => (crate :: Speculatable, &crate::SpeculatableCallbacks::DEFAULT),
        tuple_get_verifier => crate::builtin::tuple_get_verify_impl,
        tuple_get_iface_Interp => (crate :: interp :: Interp, &crate::interp::__TIR_IFACE_TupleGetOp_Interp),
        tuple_get_iface_Speculatable => (crate :: Speculatable, &crate::SpeculatableCallbacks::DEFAULT),
    },
}

pub(crate) fn make_tuple_verify_impl(
    operation: &tir::OpHandle,
    context: &Context,
) -> Result<(), Error> {
    let Some(tuple) = TupleType::view(
        context,
        context
            .get_value(crate::builtin::make_tuple_result(operation))
            .ty(),
    ) else {
        return Err(Error::VerificationError(
            "make_tuple result must have tuple type".to_string(),
        ));
    };
    let expected = tuple.elements();
    if expected.len() != operation.operands().len() {
        return Err(Error::VerificationError(format!(
            "make_tuple expected {} elements, got {}",
            expected.len(),
            operation.operands().len()
        )));
    }
    for (index, (&value, expected_type)) in operation.operands().iter().zip(expected).enumerate() {
        if context.get_value(value).ty() != expected_type {
            return Err(Error::VerificationError(format!(
                "make_tuple element {index} has the wrong type"
            )));
        }
    }
    Ok(())
}

pub fn tuple_get_tuple(operation: &tir::OpHandle) -> crate::ValueId {
    operation.operands()[0]
}

pub(crate) fn tuple_get_verify_impl(
    operation: &tir::OpHandle,
    context: &Context,
) -> Result<(), Error> {
    let Some(tuple) = TupleType::view(
        context,
        context
            .get_value(crate::builtin::tuple_get_tuple(operation))
            .ty(),
    ) else {
        return Err(Error::VerificationError(
            "tuple_get operand must have tuple type".to_string(),
        ));
    };
    let elements = tuple.elements();
    let Some(&expected) = elements.get(crate::builtin::tuple_get_index(operation) as usize) else {
        return Err(Error::VerificationError(format!(
            "tuple_get index {} is out of bounds for {} elements",
            crate::builtin::tuple_get_index(operation),
            elements.len()
        )));
    };
    if context
        .get_value(crate::builtin::tuple_get_result(operation))
        .ty()
        != expected
    {
        return Err(Error::VerificationError(
            "tuple_get result has the wrong type".to_string(),
        ));
    }
    Ok(())
}
