use std::any::Any;
use std::sync::Arc;

use crate::ty::TypeConstraint;
use crate::{Context, Error, IRFormatter, Type, TypeId, parse::Span};

use crate as tir;

/// The memory state token, written `!state`.
///
/// A `!state` value names the state of memory at a point in the program. Ops
/// that touch memory consume the state they observe and produce the state they
/// leave behind, so memory dependences are explicit def-use edges rather than an
/// implicit side channel. A state is read by any number of operations that leave
/// memory as they found it, or changed by exactly one that does not: a rewrite
/// that drops one, or hands it to a second operation that changes memory, changes
/// the program's memory order.
pub struct StateType;

impl StateType {
    /// The one `!state` id: the context interns this type first, so the id is
    /// [`TypeId::STATE`] in every context.
    #[allow(clippy::new_ret_no_self)]
    pub fn new(context: &Context) -> TypeId {
        let id = context.get_type_id(Arc::new(Self));
        debug_assert_eq!(id, TypeId::STATE);
        id
    }
}

impl TypeConstraint for StateType {}

impl Type for StateType {
    fn dialect(&self) -> &'static str {
        "builtin"
    }

    fn parse_key() -> &'static str {
        "state"
    }

    fn parse<'src>(
        _mnemonic: &str,
        _parser: &mut tir::parse::text::Parser<'src>,
        context: &Context,
    ) -> Result<TypeId, (Span, Error)> {
        Ok(Self::new(context))
    }

    fn print(&self, fmt: &mut IRFormatter<'_>) -> Result<(), std::fmt::Error> {
        fmt.write("state")
    }

    fn eq(&self, other: &dyn Type) -> bool {
        (other as &dyn Any).downcast_ref::<StateType>().is_some()
    }

    fn hash(&self, _state: &mut dyn std::hash::Hasher) {}
}
