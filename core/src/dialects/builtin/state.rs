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
/// implicit side channel. State chains are linear: a state value has at most one
/// use, and a rewrite that drops or duplicates one changes the program's memory
/// order.
pub struct StateType;

impl StateType {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(context: &Context) -> TypeId {
        context.get_type_id(Arc::new(Self))
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

/// The state ports written in an op's ` state(%in -> %out)` clause.
#[derive(Default)]
pub struct StateClause {
    /// The state the op observes.
    pub operand: Option<crate::ValueId>,
    /// The textual name bound to the state the op produces, which resolves to a
    /// value id only once the op has been built.
    pub result_name: Option<String>,
}

/// Print the ` state(%in -> %out)` clause, omitting either half that is absent.
pub fn print_state_clause(
    fmt: &mut IRFormatter<'_>,
    operand: Option<crate::ValueId>,
    result: Option<crate::ValueId>,
) -> Result<(), std::fmt::Error> {
    if operand.is_none() && result.is_none() {
        return Ok(());
    }
    fmt.write(" state(")?;
    if let Some(operand) = operand {
        fmt.write(format!("%{}", operand.number()))?;
        if result.is_some() {
            fmt.write(" ")?;
        }
    }
    if let Some(result) = result {
        fmt.write(format!("-> %{}", result.number()))?;
    }
    fmt.write(")")
}

/// Parse the clause printed by [`print_state_clause`].
pub fn parse_state_clause(
    parser: &mut tir::parse::text::Parser<'_>,
    context: &Context,
) -> Result<StateClause, (Span, Error)> {
    use tir::parse::common::Cursor;
    let mark = parser.pos();
    if !parser.parse_token("state") {
        return Ok(StateClause::default());
    }
    if !parser.parse_token("(") {
        parser.set_pos(mark);
        return Ok(StateClause::default());
    }
    let mut clause = StateClause::default();
    if let Some(name) = parser.parse_value_ref() {
        clause.operand = Some(parser.resolve_value(context, name));
    }
    if parser.parse_token("->") {
        clause.result_name = Some(
            parser
                .parse_value_ref()
                .ok_or_else(|| (parser.span(), Error::ExpectedValueRef))?
                .to_string(),
        );
    }
    if !parser.parse_token(")") {
        return Err((parser.span(), Error::ExpectedToken(")")));
    }
    Ok(clause)
}
