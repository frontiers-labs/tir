use crate::sem::Value;
use crate::utils::APInt;
use crate::{BlockId, Context, Operation, RegionId, TypeId, ValueId};

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

    fn verify_interface(
        &self,
        _this: &dyn Operation,
        _context: &Context,
    ) -> Result<(), crate::Error> {
        Ok(())
    }
}

/// A structured conditional (γ): disjoint sub-regions, one of which runs depending on
/// a deciding operand, and `k` results whose values come from the yield of whichever
/// region ran. Lets a flow-sensitive rewriter read the selection — and assume the
/// region's fact inside it — without knowing the concrete control-flow op.
pub trait Conditional {
    /// The operand deciding which region runs.
    fn decision(&self) -> ValueId;

    /// The `k` values `region` yields, aligned with the op's results.
    fn region_yields(&self, region: RegionId) -> Vec<ValueId>;

    /// For each guarded region, the value known to equal a boolean inside it
    /// (`true` => 1, `false` => 0).
    fn guarded_regions(&self) -> Vec<(RegionId, ValueId, bool)>;

    /// The n-ary reading of the selection: for each region, in region order, the value
    /// of [`Conditional::decision`] that selects it, or `None` for the region that runs
    /// when no case matches. A two-armed conditional decided by a boolean reads through
    /// its guards — the `true` arm is case 1, the `false` arm is the default.
    fn case_values(&self) -> Vec<(RegionId, Option<i64>)> {
        self.guarded_regions()
            .into_iter()
            .map(|(region, _, taken)| (region, taken.then_some(1)))
            .collect()
    }

    fn verify_interface(
        &self,
        _this: &dyn Operation,
        _context: &Context,
    ) -> Result<(), crate::Error> {
        Ok(())
    }
}

/// Relative execution cost of an operation, consulted by cost-driven rewriters
/// (e.g. InstCombine) to choose among equivalent forms. The default models one
/// cheap instruction; expensive ops override it. Exposed as an interface so the
/// cost is reachable from a `dyn Operation` without the concrete type.
pub trait OpCost {
    fn cost(&self) -> u32 {
        1
    }
    fn verify_interface(
        &self,
        _this: &dyn Operation,
        _context: &Context,
    ) -> Result<(), crate::Error> {
        Ok(())
    }
}

/// An operation that yields a compile-time constant integer, exposed generically so
/// rewriters can read the value without knowing the concrete constant op.
pub trait ConstantLike {
    fn constant_value(&self) -> APInt;
    fn verify_interface(
        &self,
        _this: &dyn Operation,
        _context: &Context,
    ) -> Result<(), crate::Error> {
        Ok(())
    }
}

/// Folds an operation over constant operands. The `operation!` macro derives this
/// automatically for any op that declares `sem` (evaluating it through the
/// semantic interpreter); ops that fold but lack a semantic expression implement it
/// by hand.
pub trait ConstantFold {
    /// `operands[i]` is the constant value of operand `i`. Returns the folded
    /// result, or `None` when this op cannot fold these operands.
    fn fold(&self, operands: &[Value]) -> Option<Value>;
    fn verify_interface(
        &self,
        _this: &dyn Operation,
        _context: &Context,
    ) -> Result<(), crate::Error> {
        Ok(())
    }
}

pub trait Commutative {
    fn is_commutative(&self) -> bool {
        true
    }
    fn verify_interface(
        &self,
        _this: &dyn Operation,
        _context: &Context,
    ) -> Result<(), crate::Error> {
        Ok(())
    }
}

/// An arithmetic operation over integer values.
pub trait IntegerArithmetic {
    fn verify_interface(
        &self,
        _this: &dyn Operation,
        _context: &Context,
    ) -> Result<(), crate::Error> {
        Ok(())
    }
}

pub trait Terminator {
    fn is_terminator(&self) -> bool {
        true
    }

    fn successors(&self) -> Vec<BlockId> {
        Vec::new()
    }

    fn verify_interface(
        &self,
        _this: &dyn Operation,
        _context: &Context,
    ) -> Result<(), crate::Error> {
        Ok(())
    }
}

/// An operation whose selected region entry arguments define non-forwarding
/// control tokens.
pub trait TokenScope {
    fn token_scope_regions(&self) -> Vec<RegionId>;

    fn verify_interface(
        &self,
        _this: &dyn Operation,
        _context: &Context,
    ) -> Result<(), crate::Error> {
        Ok(())
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

    fn verify_interface(
        &self,
        _this: &dyn Operation,
        _context: &Context,
    ) -> Result<(), crate::Error> {
        Ok(())
    }
}

/// A structured loop (θ) with `n` carried ports. Port `j` starts at `inits()[j]`,
/// enters each iteration as the body region argument `carried_args()[j]`, and updates
/// to `latched()[j]`, the value the body yields on the back edge. `finals()[j]` — the
/// op's result — is the carried value *after* the loop exits, a distinct quantity from
/// the per-iteration `carried_args()[j]`, never congruent to it. All four accessors are
/// aligned and of equal arity `n`. Lets a flow-sensitive analysis build μ and η gates
/// without knowing the concrete loop op.
pub trait LoopLike {
    /// The pre-loop initial value of each carried port.
    fn inits(&self) -> Vec<ValueId>;
    /// The body region arguments through which the carried values enter each iteration.
    fn carried_args(&self) -> Vec<ValueId>;
    /// The values the body yields on the back edge: the next iteration's carried values.
    fn latched(&self) -> Vec<ValueId>;
    /// The op's results: the carried values at loop exit.
    fn finals(&self) -> Vec<ValueId>;

    fn verify_interface(
        &self,
        _this: &dyn Operation,
        _context: &Context,
    ) -> Result<(), crate::Error> {
        let arities = [
            self.inits().len(),
            self.carried_args().len(),
            self.latched().len(),
            self.finals().len(),
        ];
        if arities.iter().any(|&n| n != arities[0]) {
            return Err(crate::Error::VerificationError(format!(
                "loop carried ports must be aligned: {} inits, {} carried arguments, \
                 {} latched values, {} results",
                arities[0], arities[1], arities[2], arities[3]
            )));
        }
        Ok(())
    }
}

/// How the two sides of an [`EntryGuard::Less`] comparison are ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardOrdering {
    Signed,
    Unsigned,
}

/// The zero-trip test of a [`GuardedLoop`], read as *structure* rather than as a value:
/// no variant names a `ValueId` that the loop does not already have as an operand or a
/// region-internal value, so a consumer can build the guard term in its own vocabulary
/// (an e-graph, a solver) without any operation being materialized in the IR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryGuard {
    /// The loop is tail-controlled: its body runs at least once, so there is no
    /// zero-trip check to build.
    AlwaysTaken,
    /// The body runs iff `lhs < rhs` under `ordering` — a counted loop's bound
    /// comparison, over the loop's existing operands. A loop counting downwards states
    /// the same test with the operands swapped; a test that is not a strict ordering
    /// belongs in [`EntryGuard::Region`].
    Less {
        ordering: GuardOrdering,
        lhs: ValueId,
        rhs: ValueId,
    },
    /// The body runs iff evaluating `region` produces 1 in `condition`, with
    /// `arguments` — the region's entry arguments, aligned with
    /// [`LoopLike::inits`] — bound to the loop's init operands.
    Region {
        region: RegionId,
        arguments: Vec<ValueId>,
        condition: ValueId,
    },
}

/// The conditional-execution reading of a [`LoopLike`]: whether its first iteration
/// runs at all. Together the two interfaces state the γ∘θ decomposition a consumer
/// needs — `If(entry_guard, Theta(init, latch), init)` per carried port — while the IR
/// keeps its source shape.
///
/// This is a sibling of [`Conditional`], not an extension of it, because the two answer
/// different questions. A `Conditional` selects among regions that exist and hands back
/// a decision *value*; a loop's zero-trip guard is a fact about the loop's operands that
/// no operation computes, so `decision() -> ValueId` has nothing to return, and neither
/// does `region_yields`/`case_values` mean anything for a loop body that runs `n` times.
/// Making loops `Conditional` would either force them to materialize a comparison — the
/// one thing this design exists to avoid — or hollow out `Conditional`'s contract for
/// its existing implementors.
pub trait GuardedLoop {
    /// The condition under which the loop's first iteration runs.
    fn entry_guard(&self) -> EntryGuard;

    fn verify_interface(
        &self,
        _this: &dyn Operation,
        _context: &Context,
    ) -> Result<(), crate::Error> {
        Ok(())
    }
}

/// A [`GuardedLoop`] whose iterations are counted: a counter starts at
/// [`CountedLoop::lower_bound`], gains [`CountedLoop::step`] once per iteration, and the
/// loop runs while the counter is below [`CountedLoop::upper_bound`] under the ordering
/// of the [`EntryGuard::Less`] the loop also publishes. Together the two interfaces state
/// the whole recurrence, so a consumer can destruct or reason about the counter — build
/// an affine view, unroll, rotate — without knowing the concrete loop op.
///
/// The counter itself is deliberately not an accessor: it is not a value of the IR. A
/// counted loop's body carries only what it was given (`scf.for`'s `iter_args`), and the
/// counter comes into existence when a consumer builds the recurrence these three
/// operands describe. Naming a `ValueId` for it would oblige every implementor to
/// materialize one, which is the thing [`EntryGuard`] exists to avoid.
pub trait CountedLoop {
    /// The counter's value on the first iteration.
    fn lower_bound(&self) -> ValueId;
    /// The bound the counter is compared against before each iteration.
    fn upper_bound(&self) -> ValueId;
    /// What the counter gains each iteration.
    fn step(&self) -> ValueId;

    fn verify_interface(
        &self,
        _this: &dyn Operation,
        _context: &Context,
    ) -> Result<(), crate::Error> {
        Ok(())
    }
}

/// A [`BranchTerminator`] whose successor edges run under a known boolean fact — e.g.
/// `cond_br %c` enters its true successor when `%c` is 1 and its false successor when
/// `%c` is 0. The CFG analog of [`Conditional`], letting a flow-sensitive analysis
/// recover the predicate gating a merge without knowing the concrete branch op.
pub trait BranchGuard {
    /// For each guarded successor edge, the value known to equal a boolean when that
    /// edge is taken (`true` => 1, `false` => 0).
    fn guarded_successors(&self) -> Vec<(BlockId, ValueId, bool)>;

    fn verify_interface(
        &self,
        _this: &dyn Operation,
        _context: &Context,
    ) -> Result<(), crate::Error> {
        Ok(())
    }
}

/// An operation whose operands and results all carry one type. Memory state ports
/// are exempt: `!state` describes a side channel, not the value being computed.
pub trait SameOperandAndResultType {
    fn verify_interface(
        &self,
        this: &dyn Operation,
        context: &Context,
    ) -> Result<(), crate::Error> {
        let types: Vec<_> = this
            .operands()
            .iter()
            .chain(this.handle().results().iter())
            .map(|&value| context.get_value(value).ty())
            .filter(|&ty| {
                (context.get_type_data(ty).as_ref() as &dyn std::any::Any)
                    .downcast_ref::<crate::builtin::StateType>()
                    .is_none()
            })
            .collect();

        if !types.windows(2).all(|pair| pair[0] == pair[1]) {
            return Err(crate::Error::VerificationError(
                "operand and result types must be the same".to_string(),
            ));
        }

        Ok(())
    }
}

/// Identifies an operation that creates a memory location eligible for local SSA
/// promotion. Implementations describe the location generically rather than tying
/// the promoting transforms to a concrete pointer dialect.
pub trait PromotableAllocation {
    /// The SSA value that names the promotable memory location.
    fn promoted_location(&self) -> ValueId;

    fn verify_interface(
        &self,
        _this: &dyn Operation,
        _context: &Context,
    ) -> Result<(), crate::Error> {
        Ok(())
    }
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

    fn verify_interface(
        &self,
        _this: &dyn Operation,
        _context: &Context,
    ) -> Result<(), crate::Error> {
        Ok(())
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

    fn verify_interface(
        &self,
        _this: &dyn Operation,
        _context: &Context,
    ) -> Result<(), crate::Error> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate as tir;
    use crate::Operation;

    tir::helpers::operation! {
        DoWhileOp {
            name: "do_while",
            dialect: "test",
            regions: R {
                body: Region {
                    single_block: true,
                }
            },
            interfaces: [crate::GuardedLoop],
        }
    }

    /// A tail-controlled loop: its body always runs once, so the guard reading says
    /// there is no zero-trip check.
    impl GuardedLoop for DoWhileOp {
        fn entry_guard(&self) -> EntryGuard {
            EntryGuard::AlwaysTaken
        }
    }

    #[test]
    fn a_tail_controlled_loop_reads_as_unconditionally_entered() {
        let context = Context::with_default_dialects();
        DoWhileOp::register_interfaces(&context);
        let region = context.create_region();
        region.add_block(context.create_block(vec![]).id());
        let op = DoWhileOpBuilder::new(&context).body(region.id()).build();

        let guard = context
            .get_op(op.id())
            .as_interface::<dyn GuardedLoop>()
            .expect("the test loop implements GuardedLoop")
            .entry_guard();

        assert_eq!(guard, EntryGuard::AlwaysTaken);
    }
}
