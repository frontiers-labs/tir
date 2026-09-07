use crate::sem::Value;
use crate::utils::APInt;
use crate::{BlockId, RegionId, TypeId, ValueId};

/// Whether a symbol may be referenced from outside the module that defines it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    Public,
    Private,
}

/// An operation that defines a named entity in the enclosing module's symbol
/// table. Functions are overloadable and key on their argument types; data
/// symbols carry no signature and own their name outright.
pub trait Symbol {
    fn symbol_name(&self) -> String;

    /// `None` for a symbol that cannot be overloaded (a global, a struct
    /// definition); `Some(argument types)` for one that keys on a signature.
    fn symbol_signature(&self) -> Option<Vec<TypeId>>;

    /// The type a call to this symbol produces; `None` for a symbol that is not
    /// callable.
    fn symbol_result_type(&self) -> Option<TypeId>;

    fn symbol_visibility(&self) -> Visibility;

    /// Whether the symbol has a body here rather than naming an external
    /// definition.
    fn is_definition(&self) -> bool;
}

/// Relative execution cost of an operation, consulted by cost-driven rewriters
/// (e.g. InstCombine) to choose among equivalent forms. The default models one
/// cheap instruction; expensive ops override it. Exposed as an interface so the
/// cost is reachable from a `dyn Operation` without the concrete type.
pub trait OpCost {
    fn cost(&self) -> u32 {
        1
    }
}

/// An operation that yields a compile-time constant integer, exposed generically so
/// rewriters can read the value without knowing the concrete constant op.
pub trait ConstantLike {
    fn constant_value(&self) -> APInt;
}

/// Folds an operation over constant operands. The `operation!` macro derives this
/// automatically for any op that declares `sem` (evaluating it through the
/// semantic interpreter); ops that fold but lack a semantic expression implement it
/// by hand.
pub trait ConstantFold {
    /// `operands[i]` is the constant value of operand `i`. Returns the folded
    /// result, or `None` when this op cannot fold these operands.
    fn fold(&self, operands: &[Value]) -> Option<Value>;
}

/// An operation that produces its results and does nothing else: no memory
/// effect, no control flow, no machine state. Dead code elimination may erase
/// one whose results go unread, whether or not it declares semantics.
pub trait Pure {}

pub trait Commutative {
    fn is_commutative(&self) -> bool {
        true
    }
}

/// An arithmetic operation over integer values.
pub trait IntegerArithmetic {}

/// An operation of an ordered region that binds the region's results: the
/// values it carries are what the region produces when control leaves through
/// it. An unordered region names those values outright instead.
pub trait RegionExit {
    fn exit_values(&self) -> Vec<ValueId>;
}

pub trait Terminator {
    fn is_terminator(&self) -> bool {
        true
    }

    fn successors(&self) -> Vec<BlockId> {
        Vec::new()
    }
}

/// A terminator that transfers control to successor blocks within the same region,
/// forwarding values to their block arguments. Lets a CFG analysis read the edge
/// targets and the values flowing along each edge without knowing the concrete
/// branch op.
pub trait BranchTerminator {
    /// One entry per outgoing control-flow edge: the successor block and the values
    /// forwarded to its block arguments, in argument order.
    fn successor_operands(&self) -> Vec<(BlockId, Vec<ValueId>)>;
}

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
pub trait CountedLoop {
    /// The counter's value on the first iteration.
    fn lower_bound(&self) -> ValueId;
    /// The bound the counter is compared against before each iteration.
    fn upper_bound(&self) -> ValueId;
    /// What the counter gains each iteration.
    fn step(&self) -> ValueId;
    /// The body port carrying the counter, for a loop whose counter is a value
    /// of the IR; `None` where the counter exists only as the recurrence above.
    fn induction(&self) -> Option<usize> {
        None
    }
}

/// A [`BranchTerminator`] whose successor edges run under a known boolean fact — e.g.
/// `cond_br %c` enters its true successor when `%c` is 1 and its false successor when
/// `%c` is 0. The CFG analog of [`Gamma`], letting a flow-sensitive analysis recover
/// the predicate gating a merge without knowing the concrete branch op.
pub trait BranchGuard {
    /// For each guarded successor edge, the value known to equal a boolean when that
    /// edge is taken (`true` => 1, `false` => 0).
    fn guarded_successors(&self) -> Vec<(BlockId, ValueId, bool)>;
}

/// An operation whose value operands and results all carry one type. The
/// dependency partitions are exempt: they describe ordering, not the value
/// being computed.
/// Checked by [`verify_opdef_operands`](crate::verify_opdef_operands).
pub trait SameOperandAndResultType {}

/// Identifies an operation that creates a memory location eligible for local SSA
/// promotion. Implementations describe the location generically rather than tying
/// the promoting transforms to a concrete pointer dialect.
pub trait PromotableAllocation {
    /// The SSA value that names the promotable memory location.
    fn promoted_location(&self) -> ValueId;
}

/// Identifies an operation that reads a value from a memory location.
pub trait MemoryRead {
    /// The memory location being read.
    fn read_location(&self) -> ValueId;
    /// The SSA value produced by the read.
    fn read_value(&self) -> ValueId;
    /// The memory state the read observes, absent until state has been threaded.
    fn state_operand(&self) -> Option<ValueId> {
        None
    }
    /// The memory state the read leaves behind, absent until state has been
    /// threaded. A read leaves memory as it found it, but still names the state
    /// after it: a later write must be ordered after the read that observes the
    /// value it overwrites.
    fn state_result(&self) -> Option<ValueId> {
        None
    }
}

/// Identifies an operation that writes a value to a memory location.
pub trait MemoryWrite {
    /// The memory location being written.
    fn write_location(&self) -> ValueId;
    /// The SSA value stored into the memory location.
    fn written_value(&self) -> ValueId;
    /// The memory state the write observes, absent until state has been threaded.
    fn state_operand(&self) -> Option<ValueId> {
        None
    }
    /// The memory state the write produces, absent until state has been threaded.
    fn state_result(&self) -> Option<ValueId> {
        None
    }
}

/// Where one carried value sits on each side of a structured op: index ranges
/// into the value partitions of the op's operands, the region's ports, the
/// region's results (the values the next iteration takes, then the values the
/// op produces when it stops) and the op's results. The ranges have one
/// length and the values at one offset share a type, which is what an op's
/// `binds:` declaration states and its verifier checks. Dependencies are not
/// declared: a loop carries `m` of them through `m` dep operands, `m` dep
/// ports, `2m` dep region results and `m` dep results, and a gamma forwards
/// its dep operands to every arm.
///
/// A gamma has no next iteration, so its `continue_` range is empty; its
/// `ports` and `exit` ranges apply to every arm alike.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Binding {
    pub operands: std::ops::Range<usize>,
    pub ports: std::ops::Range<usize>,
    pub continue_: std::ops::Range<usize>,
    pub exit: std::ops::Range<usize>,
    pub results: std::ops::Range<usize>,
}

/// A structured conditional over unordered arms (γ): the predicate picks the
/// arm, every arm reads the forwarded operands through its own ports, and the
/// chosen arm's results become the op's.
pub trait Gamma {
    fn predicate(&self) -> ValueId;
    fn arms(&self) -> Vec<RegionId>;
    /// Operands to arm ports, and arm results to op results.
    fn forwarded(&self) -> Binding;
}

/// A structured loop over an unordered body (θ): the body reads the carried
/// values through its ports and produces a predicate, the values the next
/// iteration carries, and the values the op produces once the predicate is
/// false.
pub trait Theta {
    fn body(&self) -> RegionId;
    fn carried(&self) -> Binding;
    /// The body result deciding whether another iteration runs.
    fn predicate(&self) -> ValueId;
}

/// A λ node: a function definition or declaration, read through the value a
/// call takes as its callee.
pub trait Callable {
    /// The body, absent for a declaration.
    fn body(&self) -> Option<RegionId>;
    fn value(&self) -> ValueId;
    fn params(&self) -> Vec<TypeId>;
    fn result(&self) -> TypeId;
}

/// An application of a callable to a run of the op's value operands.
pub trait Apply {
    fn callee(&self) -> ValueId;
    /// The indices of the arguments among the op's value operands.
    fn args(&self) -> std::ops::Range<usize>;
}

/// A δ node: a data object with an address, and an initializer image when it
/// defines one here.
pub trait Global {
    fn address(&self) -> ValueId;
    fn initializer(&self) -> Option<Vec<u8>>;
}

/// An operation that cannot trap, so it may run whether or not control would
/// have reached it: a loop cone may hoist it, and a lowering may evaluate it on
/// a path the source did not take.
pub trait Speculatable {}

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
pub trait NonLocalExit {
    fn target(&self) -> ExitTarget;
    fn values(&self) -> Vec<ValueId>;
}

/// Which kind of structured op a `break` or `continue` may leave.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitScopeKind {
    Loop,
    Switch,
}

/// An operation a [`NonLocalExit`] can target: what kind of scope it is, and
/// the label it carries, if any.
pub trait ExitScope {
    fn exit_scope(&self) -> ExitScopeKind;
    fn label(&self) -> Option<String> {
        None
    }
}
