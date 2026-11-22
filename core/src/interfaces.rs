use crate::ValueId;

mod generated {
    include!(concat!(env!("OUT_DIR"), "/interfaces.rs"));
}

pub use generated::{
    Commutative, ConstantLike, OpCost, Speculatable, Symbol, Terminator, Visibility,
};

/// An operation that produces its results and does nothing else: no memory
/// effect, no control flow, no machine state. Dead code elimination may erase
/// one whose results go unread, whether or not it declares semantics.
pub use generated::PureInterface;
pub use generated::PureInterface as Pure;

/// An operation computing `lhs + rhs` modulo 2^W, W the result's width: a
/// constant added to either operand moves to the result, and a zero operand
/// returns the other wherever that one has the result's type. A definition the
/// e-graph builds value identity from (`docs/design/instcombine.md`), declared
/// rather than proved, and read off the operation's name alone.
pub use generated::Additive;

/// An operation computing `lhs - rhs` modulo 2^W, W the result's width: a
/// constant added to `lhs` moves to the result, one added to `rhs` moves to it
/// negated, and a zero `rhs` returns `lhs` wherever `lhs` has the result's
/// type. Read the way [`Additive`] is.
pub use generated::Subtractive;

/// An arithmetic operation over integer values.
pub use generated::IntegerArithmetic;

/// A [`BranchTerminator`] whose successor edges run under a known boolean fact — e.g.
/// `cond_br %c` enters its true successor when `%c` is 1 and its false successor when
/// `%c` is 0. The CFG analog of [`Gamma`], letting a flow-sensitive analysis recover
/// the predicate gating a merge without knowing the concrete branch op.
pub use generated::BranchGuard;
/// A terminator that transfers control to successor blocks within the same region,
/// forwarding values to their block arguments. Lets a CFG analysis read the edge
/// targets and the values flowing along each edge without knowing the concrete
/// branch op.
pub use generated::BranchTerminator;
/// A [`BranchTerminator`] choosing its successor edge by value: edge `i` is taken
/// where the selector equals case `i`, and the last edge where it equals none.
/// The CFG analog of a [`Gamma`] naming its cases.
pub use generated::CaseGuard;
/// A [`Theta`] whose iterations are counted: a counter starts at
/// [`CountedLoop::lower_bound`], gains [`CountedLoop::step`] once per iteration, and the
/// loop runs while the counter is signed-less-than [`CountedLoop::upper_bound`]. That
/// states the whole recurrence, so a consumer can destruct or reason about the counter
/// — build an affine view, unroll, rotate — without knowing the concrete loop op.
///
/// [`CountedLoop::induction`] names the port the counter rides on where the loop has
/// one. A loop that carries only the recurrence answers `None`: the counter is then not
/// a value of the IR, and naming a `ValueId` for it would oblige the implementor to
/// materialize one.
pub use generated::CountedLoop;
/// An operation whose operands and results all carry one type.
pub use generated::SameOperandAndResultType;

/// Identifies an operation that reads a value from a memory location.
pub use generated::MemoryRead;
/// Identifies an operation that writes a value to a memory location.
pub use generated::MemoryWrite;
/// Identifies an operation that creates a memory location eligible for local SSA
/// promotion. Implementations describe the location generically rather than tying
/// the promoting transforms to a concrete pointer dialect.
pub use generated::PromotableAllocation;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResourceAccess {
    Read,
    Change,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceEffect {
    pub resource: crate::builtin::StateResource,
    pub access: ResourceAccess,
    pub observed: Vec<ValueId>,
    pub produced: Vec<ValueId>,
}

/// The execution resources an operation reads or changes.
pub use generated::ResourceEffects;
/// Where the carried values sit on each side of a structured op: index ranges
/// into the op's operands, the region's ports, the region's results (the
/// values the next iteration takes, then the values the op produces when it
/// stops) and the op's results. The ranges have one length and the values at
/// one offset share a type, which is what an op's `binds:` declaration states
/// and its verifier checks. The ranges are the declaration; a reader wants
/// the values, which [`crate::binding::carried`] and
/// [`crate::binding::state_chains`] name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Binding {
    pub operands: std::ops::Range<usize>,
    pub ports: std::ops::Range<usize>,
    pub continue_: std::ops::Range<usize>,
    pub exit: std::ops::Range<usize>,
    pub results: std::ops::Range<usize>,
}

/// An application of a callable to a run of the op's value operands.
pub use generated::Apply;
/// A λ node: a function definition or declaration, read through the value a
/// call takes as its callee.
pub use generated::Callable;
/// An operation whose results, the states it publishes among them, are a
/// function of its operands, attributes and the states it takes, though no
/// semantics says which: two over equal inputs are one, so a rewrite may match
/// it and replace it as a term.
pub use generated::Congruent;
/// A structured conditional over unordered arms (γ): the predicate picks the
/// arm, every arm reads the forwarded operands through its own ports, and the
/// chosen arm's results become the op's.
pub use generated::Gamma;
/// A δ node: a data object with an address, and an initializer image when it
/// defines one here.
pub use generated::Global;
/// An operation that maps explicit operands to one region's ports and that
/// region's yields to its results without loop or branch control flow.
pub use generated::RegionBinding;
/// A structured loop over an unordered body (θ): the body reads the carried
/// values through its ports and produces a predicate, the values the next
/// iteration carries, and the values the op produces once the predicate is
/// false.
pub use generated::Theta;

/// What a non-local exit leaves: the loop or switch nearest around it, or the
/// enclosing scope carrying `label`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExitTarget {
    InnermostLoop,
    InnermostSwitch,
    Label(String),
}

/// An operation leaving an enclosing structured op from inside its subtree
/// (a `break` or `continue`), naming the target by kind or label rather than by
/// a token value, and carrying the values the target's ports take.
pub use generated::NonLocalExit;
/// Which kind of structured op a `break` or `continue` may leave.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitScopeKind {
    Loop,
    Switch,
}

/// An operation a [`NonLocalExit`] can target: what kind of scope it is, and
/// the label it carries, if any.
pub use generated::ExitScope;
