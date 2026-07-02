/// Capability a graph-node label needs to take part in pattern matching /
/// e-matching: leaf classification, equality against a template label, and the
/// operand-kind predicates. Kept separate from [`crate::Dag`], which is a
/// pure storage view, so analyses that never match patterns (e.g. dominator trees)
/// owe it nothing.
pub trait Matchable<C> {
    fn is_leaf(&self, ctx: &C) -> bool;

    fn num_children(&self, ctx: &C) -> usize;

    fn matches_pattern(&self, template: &Self, _ctx: &C) -> bool
    where
        Self: PartialEq + Sized,
    {
        self == template
    }

    fn is_commutative(&self) -> bool {
        false
    }

    /// Whether this node is a compile-time constant, distinguishing immediate
    /// operands (which must bind to a constant) from register operands (which must
    /// not) during matching.
    fn is_constant(&self) -> bool {
        false
    }
}

/// Restricts what a boundary (operand) pattern node may bind to, distinguishing
/// register operands from immediates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperandConstraint {
    /// Must bind to a non-constant value (a register / SSA value).
    Register,
    /// Must bind to a compile-time constant (an immediate).
    Immediate,
}
