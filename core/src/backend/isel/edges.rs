//! How a selected function's blocks are joined: the target's branches, read
//! off the automaton's `branches`, and the function's own return.
//!
//! A control whose predicate the commit left to it gets the branch of the
//! chain extracted for the predicate's class, over the registers the
//! predicate's operation reads and the constants the chain's form holds.
//! Any other predicate is tested through the same table: the branch of the
//! compare for equality with the case, which for a boolean is the inverse of
//! the compare with zero.

use smallvec::SmallVec;
use tir::sem::SemNode;
use tir::{BlockId, Context, IdMap, OpId, PassError, TypeId, ValueId};
use tir_adt::APInt;
use tir_egraph::{Engine, Extraction, Ref, RowId};

use super::automaton::{Automaton, Pattern, Seed, Tree, canonical_type};
use super::rule::BranchEmitters;
use super::select::{Bound, Commit, Emit, Emitted, Operands, Slot, fits};
use crate::passes::destructure::{
    CfgEdges, ControlDefinition, ControlOutcome, Edge, Edges, Test, ValueBinding,
};

// A compare for equality of a register in state `seed`, `width` bits wide,
// with a constant of `class`, the last cells of an `imm` row, which is child
// `constant` of the row and held by a field; `None` for the compare of two
// registers. `cost` is its branch's.
struct Case {
    seed: Pattern,
    width: u32,
    class: Option<[u32; 8]>,
    constant: u8,
    pattern: Pattern,
    cost: u32,
}

/// The compares a control tests a predicate by where the function holds
/// none: per seed of a predicate, the patterns with a branch that compare
/// it for equality with a case, the ones holding the case in a field first,
/// the cheaper branch first among those.
#[derive(Default)]
pub struct Cases {
    cases: Vec<Case>,
}

impl Cases {
    /// The cases of `automaton`, where `compares` has, per seed of a
    /// predicate, the label of the row that compares it for equality and
    /// the predicate's width.
    pub fn of(automaton: &Automaton, compares: &[(Pattern, u32, u32)]) -> Self {
        let (Some(step), Some(imm)) = (automaton.step.get(2), automaton.imm.get(2)) else {
            return Self::default();
        };
        let constant = |word: u32| {
            let seeds = automaton.seeds.iter().zip(&automaton.chains);
            seeds
                .filter(|(seed, _)| matches!(seed, Seed::Constant(_)))
                .any(|(_, &chain)| chain == word)
        };
        let class = |key: [u32; 3], child: u32, class: u32| {
            (0..imm.len()).find_map(|row| {
                let cell = |column: usize| imm.column(column)[row];
                let keyed = [cell(0), cell(1), cell(2)] == key && cell(3) == child;
                (keyed && cell(4) == class).then(|| std::array::from_fn(|at| cell(5 + at)))
            })
        };
        let mut cases = Vec::new();
        for &(seed, label, width) in compares {
            let register = automaton.chains[seed as usize];
            for row in (0..step.len()).filter(|&row| step.column(0)[row] == label) {
                let cell = |column: usize| step.column(column)[row];
                let (key, pattern) = ([label, cell(1), cell(2)], cell(5));
                let Some(branch) = automaton.branch(pattern) else {
                    continue;
                };
                let at = |child: u32| class(key, child, cell(3 + child as usize));
                let still = |child: u32| at(child).is_some_and(|class| fits(&class, 0));
                // A constant no field holds is built, which is the compare
                // of two registers.
                let held = |child: u32| automaton.field[pattern as usize] & (1 << child) != 0;
                let case = match (cell(1) == register, cell(2) == register) {
                    (true, true) if still(0) && still(1) => Some((None, 1)),
                    (true, false) if still(0) && constant(cell(2)) && held(1) => {
                        at(1).map(|by| (Some(by), 1))
                    }
                    (false, true) if still(1) && constant(cell(1)) && held(0) => {
                        at(0).map(|by| (Some(by), 0))
                    }
                    _ => None,
                };
                cases.extend(case.map(|(class, constant)| Case {
                    seed,
                    width,
                    class,
                    constant,
                    pattern,
                    cost: branch.cost,
                }));
            }
        }
        cases.sort_by_key(|case| (case.class.is_none(), case.cost));
        Self { cases }
    }

    /// What the branch on a predicate `width` bits wide being zero costs.
    pub fn tested(&self, width: u32) -> Option<u32> {
        let zero = |held: &&Case| held.width == width && held.class.is_none_or(|by| fits(&by, 0));
        self.cases.iter().find(zero).map(|held| held.cost)
    }

    // The pattern comparing a register in state `seed` with `case`, and
    // which child of its row the case is.
    fn test(&self, seed: Pattern, case: u64) -> Option<(Pattern, u8)> {
        let holds =
            |held: &&Case| held.seed == seed && held.class.is_none_or(|class| fits(&class, case));
        let held = self.cases.iter().find(holds)?;
        Some((held.pattern, held.constant))
    }
}

pub(super) struct Machine<'a> {
    pub context: &'a Context,
    pub emitters: &'a BranchEmitters,
    pub commit: &'a Commit<'a>,
    pub emitted: &'a Emitted,
    /// The graph, the classes of the function's values in it, and its
    /// extraction.
    pub graph: (&'a Engine<SemNode>, &'a IdMap<ValueId, Ref>),
    pub extraction: &'a Extraction<'a, SemNode>,
    /// The type each predicate no instruction defines had when recovery
    /// first asked: the first branch to read it gives it a register class.
    pub types: std::cell::RefCell<IdMap<ValueId, TypeId>>,
    /// What each operation left for a branch read when recovery first
    /// asked: recovery renames an operation's operands as it places it.
    pub reads: std::cell::RefCell<IdMap<OpId, Vec<ValueId>>>,
}

impl Machine<'_> {
    fn go(&self, block: BlockId, dest: BlockId, args: &[ValueId]) {
        let jump = (self.emitters.uncond)(self.context, dest, args);
        self.context.get_block(block).append(jump.id());
    }

    fn source_type(&self, value: ValueId) -> TypeId {
        let source = self.emitted.types.get(&value).copied();
        source.unwrap_or_else(|| {
            let mut types = self.types.borrow_mut();
            let current = || self.context.get_value(value).ty();
            *types.entry(value).or_insert_with(current)
        })
    }

    // The registers the operation the commit left for the branch on the
    // boolean `source` reads, with the form and the pattern of the chain
    // extracted for it.
    fn left(&self, source: ValueId) -> Option<(Vec<ValueId>, RowId, Pattern)> {
        let context = self.context;
        let replaced = |op: &OpId| self.commit.replaces(context, &context.get_op(*op));
        // The instruction computing a value follows the operation left
        // for the branch on it.
        let computed = context.get_value(source).defining_op()?;
        let followed = self.emitted.implicit.get(&computed);
        let followed = followed.into_iter().flatten().copied();
        let producer = std::iter::once(computed).chain(followed).find(replaced)?;
        let source = context.get_op(producer).results().first().copied()?;
        let (form, pattern) = self.commit.tested(self.graph, self.extraction, source)?;
        let mut reads = self.reads.borrow_mut();
        let read = || context.get_op(producer).operands().to_vec();
        let registers = reads.entry(producer).or_insert_with(read).clone();
        Some((registers, form, pattern))
    }

    // The branch deciding `outcome` of `control` and its inverse, over the
    // operands they read, and whether the outcome is the branch taken.
    fn test(
        &self,
        control: &ControlDefinition,
        outcome: usize,
    ) -> Result<(&Tree, Option<&Tree>, Operands, bool), PassError> {
        let (context, select) = (self.context, &self.commit.select);
        let automaton = select.automaton;
        let source = control.source_predicate;
        let ty = self.source_type(source);
        let width = context.terms().bit_width(ty);
        let boolean = width == Some(1);
        let unstated =
            || PassError::InvalidRuleSet("a control outcome states nothing to branch on".into());
        let (case, holds) = match (control.outcomes.get(outcome), width) {
            (Some(ControlOutcome::Exact(case)), Some(1)) => (0, *case == 0),
            (Some(ControlOutcome::DefaultFrom(_)), Some(1)) => (0, false),
            (Some(ControlOutcome::Exact(case)), _) => (*case, true),
            _ => return Err(unstated()),
        };
        let branch = |pattern: Pattern| {
            let unbranched =
                || PassError::InvalidRuleSet(format!("pattern {pattern} has no branch"));
            automaton.branch(pattern).ok_or_else(unbranched)
        };
        if let Some((registers, form, pattern)) = self.left(source).filter(|_| boolean) {
            let unbound = |why: &str| {
                PassError::InvalidRuleSet(format!("the branch of pattern {pattern} reads {why}"))
            };
            let branch = branch(pattern)?;
            let inverse = branch.inverse.as_ref();
            let accept = automaton.accept[pattern as usize].as_ref();
            let accept = accept.ok_or_else(|| unbound("a pattern that accepts nothing"))?;
            let trees = std::iter::once(&branch.taken).chain(inverse);
            let slots = self.commit.slots(self.graph.0, form, accept, trees, false);
            let mut registers = registers.into_iter();
            let mut operands: Operands = SmallVec::from_elem(None, accept.operands.len());
            for (operand, slot) in slots.map_err(unbound)? {
                let bound = match slot {
                    Slot::Immediate(value) => Bound::Immediate(value),
                    Slot::Register(_) => {
                        let register = registers.next();
                        Bound::Register(register.ok_or_else(|| unbound("a register too many"))?)
                    }
                };
                operands[usize::from(operand)] = Some(bound);
            }
            return Ok((&branch.taken, inverse, operands, !holds));
        }
        let unruled = || {
            let ty = context.type_to_string(ty);
            PassError::InvalidRuleSet(format!(
                "no rule branches on a {ty} predicate compared with {case}"
            ))
        };
        let seed = Seed::Register(canonical_type(context, ty));
        let seed = automaton.seeds.iter().position(|known| *known == seed);
        let seed = seed.ok_or_else(unruled)? as Pattern;
        let (pattern, constant) = select.cases.test(seed, case).ok_or_else(unruled)?;
        let accept = automaton.accept[pattern as usize].as_ref();
        let width = width.ok_or_else(unruled)? as u32;
        let case = APInt::new_signed(u64::from(width), tir::binding::signed_bits(case, width));
        let binds = accept.ok_or_else(unruled)?.operands.iter();
        let operands = binds.map(|bind| match bind.path.first() == Some(&constant) {
            true => Some(Bound::Immediate(case.clone())),
            false => Some(Bound::Register(source)),
        });
        let operands = operands.collect();
        let branch = branch(pattern)?;
        // A boolean holds where its compare with zero does not, so the
        // branch on it is that compare's inverse.
        Ok(match (&branch.inverse, boolean) {
            (Some(inverse), true) => (inverse, Some(&branch.taken), operands, !holds),
            (inverse, _) => (&branch.taken, inverse.as_ref(), operands, holds),
        })
    }
}

impl Edges for Machine<'_> {
    fn jump(&self, block: BlockId, edge: &Edge) {
        self.go(block, edge.dest, &edge.args);
    }

    fn branch(
        &self,
        _block: BlockId,
        _op: &tir::OpHandle,
        _test: Test,
        _taken: &Edge,
        _fallthrough: &Edge,
        _mint: &mut dyn FnMut() -> BlockId,
    ) -> Result<(), PassError> {
        Err(PassError::InvalidRuleSet(
            "machine CFG recovery requires recorded control definitions".into(),
        ))
    }

    fn branch_control(
        &self,
        control: &ControlDefinition,
        outcome: usize,
        _predicate: ValueId,
        bindings: &[ValueBinding],
        block: BlockId,
        taken: &Edge,
        fallthrough: &Edge,
        mint: &mut dyn FnMut() -> BlockId,
    ) -> Result<(), PassError> {
        let context = self.context;
        let (branch, inverse, mut operands, holds) = self.test(control, outcome)?;
        // The branch goes where its compare holds. The inverse goes the
        // other way where that edge alone continues a loop, so the loop
        // branches back and its exit falls through.
        let (held, other) = match holds {
            true => (taken, fallthrough),
            false => (fallthrough, taken),
        };
        let (tree, taken, fallthrough) = match inverse {
            Some(inverse) if other.loops_back && !held.loops_back => (inverse, other, held),
            _ => (branch, held, other),
        };
        // A taken edge has no operand slot to carry assignments in, so one
        // that performs any goes through a block only it reaches.
        let dependencies = context.get_block(taken.dest).state_arguments().len();
        let target = match taken.args.len() > dependencies {
            true => {
                let hop = mint();
                self.go(hop, taken.dest, &taken.args);
                hop
            }
            false => taken.dest,
        };
        // The bindings are one simultaneous substitution.
        let current = |value: ValueId| {
            let bound = bindings.iter().find(|binding| binding.source == value);
            bound.map_or(value, |binding| binding.current)
        };
        for operand in operands.iter_mut().flatten() {
            if let Bound::Register(value) = operand {
                *value = current(*value);
            }
        }
        let holder = context.get_block(block);
        let mut place = |op: OpId, _: Option<OpId>| holder.append(op);
        let emit = Emit {
            context,
            ty: self.source_type(control.source_predicate),
            ports: &[],
            target: Some(target),
            lanes: None,
            place: &mut place,
        };
        self.commit.steps(emit, tree, &operands)?;
        self.go(block, fallthrough.dest, &fallthrough.args);
        Ok(())
    }

    fn decided_control(&self, control: &ControlDefinition, outcome: usize) -> Option<bool> {
        let value = *self.emitted.literals.get(&control.source_predicate)?;
        match control.outcomes.get(outcome)? {
            ControlOutcome::Exact(case) => Some(value == *case),
            ControlOutcome::DefaultFrom(first) => Some(value >= *first as u64),
            ControlOutcome::Rest => None,
        }
    }

    // The branch of a compare reads what the compare's operation was left
    // reading, not its value.
    fn control_reads(&self, control: &ControlDefinition, _outcome: usize) -> Vec<ValueId> {
        let source = control.source_predicate;
        let boolean = self.context.terms().bit_width(self.source_type(source)) == Some(1);
        match self.left(source).filter(|_| boolean) {
            Some((registers, ..)) => registers,
            None => vec![source],
        }
    }

    fn constant(&self, value: ValueId) -> Option<u64> {
        self.emitted.literals.get(&value).copied()
    }

    fn is_literal(&self, op: OpId) -> bool {
        let defined = self.context.get_op(op).results();
        let mut defined = defined.iter();
        defined.any(|value| self.emitted.literals.contains_key(value))
    }

    fn implicit_inputs(&self, op: OpId) -> Vec<OpId> {
        self.emitted.implicit.get(&op).cloned().unwrap_or_default()
    }

    fn compatible_type(&self, source: TypeId, selected: TypeId) -> bool {
        source == selected
            || (!self.context.is_state_type(source)
                && crate::backend::registers::RegClassType::view(self.context, selected).is_some())
    }

    fn leave(&self, block: BlockId, values: &[ValueId], deps: &[ValueId]) -> Result<(), PassError> {
        CfgEdges {
            context: self.context,
        }
        .leave(block, values, deps)
    }
}
