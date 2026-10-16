use tir_symbolic::term::ctor;

use crate::parse::common::Cursor;
use crate::ty::{Terms, TypeArg, TypeSpelling};
use crate::{Context, Error, Operation, TypeId, operation};

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
        TypeSpelling {
            ctor: ctor::TUPLE,
            keys: &["tuple"],
            parse: |_, parser, context| {
                let elements = parser
                    .parse_delimited("<", ">", |parser| {
                        parser
                            .parse_type(context)?
                            .ok_or_else(|| (parser.span(), Error::ExpectedType))
                    })?
                    .ok_or_else(|| (parser.span(), Error::ExpectedToken("<")))?;
                Ok(Self::new(context, elements))
            },
            print: |spellings, ty, fmt| {
                fmt.write("tuple<")?;
                let tuple = Self::view(spellings, ty).expect("a tuple type");
                for (index, element) in tuple.0.into_iter().enumerate() {
                    if index > 0 {
                        fmt.write(", ")?;
                    }
                    spellings.print(element, fmt)?;
                }
                fmt.write(">")
            },
            variadic_tail: false,
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

operation! {
    MakeTupleOp {
        name: "make_tuple",
        dialect: "builtin",
        scheme: "(_*) -> tuple<_>",
        verifier: "true",
        operands: O { elements },
        results: R { result },
        interfaces: [crate::interp::Interp, crate::Speculatable],
    }
}

impl crate::Speculatable for MakeTupleOp {}

impl tir::Verifiable for MakeTupleOp {
    fn verify_impl(&self, context: &Context) -> Result<(), Error> {
        let Some(tuple) = TupleType::view(context, context.get_value(self.result()).ty()) else {
            return Err(Error::VerificationError(
                "make_tuple result must have tuple type".to_string(),
            ));
        };
        let expected = tuple.elements();
        if expected.len() != self.operands().len() {
            return Err(Error::VerificationError(format!(
                "make_tuple expected {} elements, got {}",
                expected.len(),
                self.operands().len()
            )));
        }
        for (index, (&value, expected_type)) in self.operands().iter().zip(expected).enumerate() {
            if context.get_value(value).ty() != expected_type {
                return Err(Error::VerificationError(format!(
                    "make_tuple element {index} has the wrong type"
                )));
            }
        }
        Ok(())
    }
}

operation! {
    TupleGetOp {
        name: "tuple_get",
        dialect: "builtin",
        scheme: "(tuple<_>) -> _",
        verifier: "true",
        operands: O { tuple },
        attributes: A {
            index: "UInt",
        },
        results: R { result },
        interfaces: [crate::interp::Interp, crate::Speculatable],
    }
}

impl crate::Speculatable for TupleGetOp {}

impl TupleGetOp {
    pub fn tuple(&self) -> crate::ValueId {
        self.operands()[0]
    }
}

impl tir::Verifiable for TupleGetOp {
    fn verify_impl(&self, context: &Context) -> Result<(), Error> {
        let Some(tuple) = TupleType::view(context, context.get_value(self.tuple()).ty()) else {
            return Err(Error::VerificationError(
                "tuple_get operand must have tuple type".to_string(),
            ));
        };
        let elements = tuple.elements();
        let Some(&expected) = elements.get(self.index() as usize) else {
            return Err(Error::VerificationError(format!(
                "tuple_get index {} is out of bounds for {} elements",
                self.index(),
                elements.len()
            )));
        };
        if context.get_value(self.result()).ty() != expected {
            return Err(Error::VerificationError(
                "tuple_get result has the wrong type".to_string(),
            ));
        }
        Ok(())
    }
}
