use std::any::Any as StdAny;
use std::hash::Hasher;

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
    /// Additional mnemonic keys this type's parser registers under, for types
    /// whose spellings do not share one prefix (e.g. floats parse as both
    /// `f...` and `bf...`).
    fn extra_parse_keys() -> &'static [&'static str]
    where
        Self: Sized,
    {
        &[]
    }
    fn parse<'src>(
        mnemonic: &str,
        parser: &mut IRParser<'src>,
        context: &Context,
    ) -> Result<TypeId, (Span, Error)>
    where
        Self: Sized;
    fn print(&self, fmt: &mut IRFormatter<'_>) -> Result<(), std::fmt::Error>;
    /// Whether this type stands for "any number of further arguments" when it
    /// ends a function signature. Overload resolution then matches the fixed
    /// prefix and accepts any tail, including an empty one.
    fn is_variadic_tail(&self) -> bool {
        false
    }
    /// Structural equality. A type built from another type holds the context's
    /// interned `Arc` for it (see [`Context::get_type_data`]), so nested types
    /// compare by address rather than by walking the nesting — otherwise
    /// interning a deep chain costs time and stack proportional to its depth.
    fn eq(&self, other: &dyn Type) -> bool;
    /// Hash of everything [`Type::eq`] compares, so the context can bucket
    /// candidate types instead of scanning every interned type. Parameterless
    /// types contribute nothing; the context settles collisions with
    /// [`Type::eq`].
    fn hash(&self, state: &mut dyn Hasher);
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
    fn satisfies(ty: &dyn Type) -> bool
    where
        Self: Sized + 'static,
    {
        (ty as &dyn StdAny)
            .downcast_ref::<crate::builtin::TokenType>()
            .is_none()
    }
}

impl TypeId {
    /// The type of a dependency value: a reserved id the interner never hands
    /// out, so [`crate::Value::ty`] stays total. A dependency carries no bits
    /// and prints without a type; only the printer and the verifier compare
    /// against this.
    pub const DEPENDENCY: TypeId = TypeId(u32::MAX);

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
