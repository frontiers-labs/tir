use tir_symbolic::term::ctor;

use crate::parse::common::Cursor;
use crate::ty::{Terms, TypeArg, TypeSpelling};
use crate::{Context, Error, Operation, TypeId};

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

__cdsl_builtin_MakeTupleOp!();

impl crate::Speculatable for MakeTupleOp {}

__cdsl_builtin_TupleGetOp!();

impl crate::Speculatable for TupleGetOp {}

impl TupleGetOp {
    pub fn tuple(&self) -> crate::ValueId {
        self.operands()[0]
    }
}
