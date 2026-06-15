use std::any::Any as StdAny;

use crate::{
    Context, Error, IRFormatter,
    parse::{Span, text::Parser as IRParser},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TypeId(u32);

pub type TypeParser =
    for<'src> fn(&str, &mut IRParser<'src>, &Context) -> Result<TypeId, (Span, Error)>;

pub trait Type: StdAny + Sync + Send + TypeConstraint {
    fn dialect(&self) -> &'static str;
    fn parse_key() -> &'static str
    where
        Self: Sized;
    fn parse<'src>(
        mnemonic: &str,
        parser: &mut IRParser<'src>,
        context: &Context,
    ) -> Result<TypeId, (Span, Error)>
    where
        Self: Sized;
    fn print(&self, fmt: &mut IRFormatter<'_>) -> Result<(), std::fmt::Error>;
    fn eq(&self, other: &dyn Type) -> bool;
}

pub trait TypeConstraint {
    fn satisfies(ty: &dyn Type) -> bool
    where
        Self: Sized + 'static,
    {
        (ty as &dyn StdAny).downcast_ref::<Self>().is_some()
    }
}

pub struct Any;

impl TypeConstraint for Any {
    fn satisfies(_ty: &dyn Type) -> bool
    where
        Self: Sized + 'static,
    {
        true
    }
}

impl TypeId {
    pub(crate) fn as_index(self) -> usize {
        self.0 as usize
    }

    pub fn number(self) -> u32 {
        self.0
    }

    pub fn from_number(n: u32) -> Self {
        Self(n)
    }
}

impl From<u32> for TypeId {
    fn from(value: u32) -> Self {
        Self(value)
    }
}
