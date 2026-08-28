//! InstCombine: an equality-saturation simplifier. It seeds the function's
//! regions ([`seed`], which reads gates off the ops' own interfaces) into a
//! [`tir_symbolic`] e-graph of real IR values, saturates, extracts the cheapest
//! form per value by [`crate::OpCost`], and rewrites what improved.
//!
//! Flow-sensitive facts ride the e-graph's scoped assumptions, both ways round. A
//! structured region pushes its guard's condition around its body and pops it on
//! the way back out; a loop's carried port is *hypothesised* to hold the constant
//! the loop was entered on, and what no edge back into it refutes is promoted
//! into the base graph. The region tree is the dominance, so nothing here
//! computes one.
//!
//! What a rewrite leaves behind is the same commit's business: the operation it
//! took the readers of, and every operation only that one read, are erased on the
//! way out.
//!
//! The engine holds no op-specific knowledge — identity, cost, folding and
//! constant-reading come from op interfaces; op construction is owned by the rewrites.

pub(crate) mod rules;
mod seed;
mod state;

use seed::{LoopPorts, Port};

use std::collections::HashMap;

use tir_symbolic::egraph::{EGraph, Extraction, Id};

use crate::analysis::scopes;
use crate::{
    AnalysisManager, BlockId, Conditional, ConstantLike, Context, EntryGuard, GuardedLoop,
    LoopLike, MemoryRead, MemoryWrite, OpId, OperationRef, Pass, PassError, PassTarget, RegionId,
    Rewriter, TypeId, ValueId,
    attributes::AttributeValue,
    builtin::{StateType, ops},
    func::FuncOp,
    utils::APInt,
};

use crate::sem::node::cost;
use crate::sem::{Prov, SemNode as Node, SymKind};
use rules::{Ruleset, builtin_ruleset};

const ITER_LIMIT: usize = 30;
const NODE_LIMIT: usize = 100_000;

#[derive(Default)]
pub struct InstCombinePass;

impl InstCombinePass {
    pub fn new() -> Self {
        Self
    }
}

crate::register_pass!(InstCombinePass, "instcombine");

impl Pass for InstCombinePass {
    fn name(&self) -> &'static str {
        "instcombine"
    }

    fn target(&self) -> PassTarget {
        PassTarget::operation::<FuncOp>()
    }

    fn run(
        &mut self,
        op: &OperationRef,
        context: &Context,
        rewriter: &mut Rewriter,
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        if op.as_op::<FuncOp>().is_none() {
            return Ok(());
        }
        let root = op.op().id;
        let mut seeded = seed::seed(context, root);
        let loop_ports = std::mem::take(&mut seeded.loop_ports);
        let ruleset = builtin_ruleset(context, &seeded);
        let mut driver = Driver {
            context,
            eg: seeded.eg,
            value_class: seeded.value_class,
            arg_block: seeded.arg_block,
            ruleset,
            replacing: std::cell::Cell::new(None),
        };
        driver.hypothesize(loop_ports);
        let body = context.get_op(root).regions()[0];
        driver.process_region(body, rewriter)?;
        driver.sweep(root, rewriter)
    }
}

/// Rewrites each region under the assumptions that hold there, and *before* its
/// children's scopes open so the base classes a child scope reads are final.
struct Driver<'a> {
    context: &'a Context,
    eg: EGraph<Node>,
    value_class: HashMap<ValueId, Id>,
    /// The block each block argument belongs to: it has no defining op, so the
    /// scope check has no other way to place it.
    arg_block: HashMap<ValueId, BlockId>,
    ruleset: Ruleset,
    /// The value whose readers are being rewired. It answers for its own class
    /// and would answer every rewrite with itself, so no spelling may pick it.
    replacing: std::cell::Cell<Option<ValueId>>,
}

impl Driver<'_> {
    fn process_region(
        &mut self,
        region: RegionId,
        rewriter: &mut Rewriter,
    ) -> Result<(), PassError> {
        self.eg
            .saturate(&self.ruleset.rewrites, ITER_LIMIT, NODE_LIMIT);
        crate::memstats::egraph_census("instcombine", &self.eg);
        let extraction = self.eg.extract_best(|_, node| cost(node));

        let blocks: Vec<crate::BlockHandle> = self
            .context
            .get_region(region)
            .iter(self.context.clone())
            .collect();
        // A carried port is a block argument, and what its class turns out to be
        // — a constant a hypothesis proved, say — is read at every use of it.
        for block in &blocks {
            let Some(&first) = block.op_ids().first() else {
                continue;
            };
            let target = self.at(first);
            for argument in block.arguments() {
                self.rewire(argument.id(), &extraction, &target, rewriter)?;
            }
        }
        let op_ids: Vec<OpId> = blocks.iter().flat_map(|block| block.op_ids()).collect();
        for &op_id in &op_ids {
            self.rewrite_op(op_id, &extraction, rewriter)?;
        }
        self.recurse(&op_ids, rewriter)
    }

    /// Erase what the rewrites left behind. A rewrite reroutes the readers of a
    /// value, and the operation that computed it — and every operation only that
    /// one read — is dead from that moment: the cascade belongs to the commit,
    /// not to a pass after it.
    fn sweep(&self, root: OpId, rewriter: &mut Rewriter) -> Result<(), PassError> {
        let defuse = crate::analysis::DefUse::new(self.context, root);
        super::dce::erase_dead(self.context, rewriter, &defuse)
    }

    /// Replace the readers of `value` with the cheapest form of its class, where
    /// that form is in scope at `target`. The operation defining it stays: this
    /// is what a value an op does not name alone — a port, a block argument, one
    /// result of many — is worth to whoever reads it.
    fn rewire(
        &self,
        value: ValueId,
        extraction: &Extraction<Node>,
        target: &OperationRef,
        rewriter: &mut Rewriter,
    ) -> Result<(), PassError> {
        let ty = self.context.get_value(value).ty();
        if ty == StateType::new(self.context) {
            return Ok(());
        }
        let Some(&class) = self.value_class.get(&value) else {
            return Ok(());
        };
        self.replacing.set(Some(value));
        let spelled =
            self.materialize(extraction, class, ty, target, rewriter, &mut HashMap::new());
        self.replacing.set(None);
        let Some(new_value) = spelled? else {
            return Ok(());
        };
        if new_value != value && self.dominates_op(new_value, target.op().id) {
            self.replace_reads(value, new_value, target);
        }
        Ok(())
    }

    /// Rewire the readers of `value` under the region `target` sits in, except
    /// the edges: a region exit forwarding `value` already carries it in a
    /// register, and a literal there buys an instruction for nothing.
    fn replace_reads(&self, value: ValueId, new_value: ValueId, target: &OperationRef) {
        let Some(region) = self
            .context
            .get_op(target.op().id)
            .parent_block()
            .and_then(|block| self.context.parent_region(block))
        else {
            return;
        };
        let mut pending = vec![region];
        while let Some(region) = pending.pop() {
            for block in self.context.get_region(region).iter(self.context.clone()) {
                for op_id in block.op_ids() {
                    let op = self.context.get_op(op_id);
                    pending.extend(op.regions());
                    if scopes::region_exit_kind(&op).is_some() {
                        continue;
                    }
                    let operands = op.operands();
                    if operands.contains(&value) {
                        let rebound = operands
                            .iter()
                            .map(|&operand| if operand == value { new_value } else { operand })
                            .collect();
                        self.context.set_op_operands(op_id, rebound);
                    }
                }
            }
        }
    }

    /// A cursor at `op`, for a rewrite to build in front of.
    fn at(&self, op: OpId) -> OperationRef {
        let instance = self.context.get_op(op);
        let block = instance.parent_block().map(|id| self.context.get_block(id));
        OperationRef::new(instance, block, None)
    }

    /// Replace `op_id`'s value with its cheapest equivalent form, if that improved.
    fn rewrite_op(
        &self,
        op_id: OpId,
        extraction: &Extraction<Node>,
        rewriter: &mut Rewriter,
    ) -> Result<(), PassError> {
        if !self.context.has_operation(op_id) {
            return Ok(());
        }
        let instance = self.context.get_op(op_id);
        // A constant materializes to itself.
        if instance
            .clone()
            .as_interface::<dyn ConstantLike>()
            .is_some()
        {
            return Ok(());
        }
        // A read leaves memory as it found it, so the state it publishes is the
        // state it read: its uses reroute to that operand and the read goes with
        // its value. Nothing else multi-result names one value to replace.
        let state_edge = instance
            .clone()
            .as_interface::<dyn MemoryRead>()
            .and_then(|read| match (read.state_operand(), read.state_result()) {
                (Some(operand), Some(result)) => Some((read.read_value(), operand, result)),
                _ => None,
            });
        let value = match (state_edge, instance.results().as_slice()) {
            (Some((value, ..)), _) => value,
            (None, [value]) => *value,
            // A gate, a loop or a call names no single value to replace, but each
            // of its results is worth what its class is worth to whoever reads it.
            (None, results) => {
                let target = self.at(op_id);
                for &result in results {
                    self.rewire(result, extraction, &target, rewriter)?;
                }
                return Ok(());
            }
        };
        let Some(&class) = self.value_class.get(&value) else {
            return Ok(());
        };
        let ty = self.context.get_value(value).ty();
        let block = instance.parent_block().map(|b| self.context.get_block(b));
        let target = OperationRef::new(instance.clone(), block, None);
        let mut memo = HashMap::new();
        let Some(new_value) =
            self.materialize(extraction, class, ty, &target, rewriter, &mut memo)?
        else {
            return Ok(());
        };
        // Handing this op's readers another state is only sound when the op is
        // erased, changes memory, and the state handed on is the very one it
        // consumed — its use then moves rather than doubles. A gate keeping its
        // regions, or a chain named anywhere else, would be consumed twice. A
        // `state.join` is the case that looks sound and is not: it names the memory
        // after every read of a fork, and the reads it stands for are still there
        // naming the state before it, so handing that state on would leave the
        // write taking it unordered against them.
        if ty == StateType::new(self.context)
            && !(instance.regions().is_empty()
                && instance.has_interface::<dyn MemoryWrite>()
                && instance.operands().contains(&new_value))
        {
            return Ok(());
        }
        // The replacement must dominate the use it takes over. Operand reuse and
        // freshly built ops satisfy this by construction; a cross-block CSE or a gate
        // collapsing to an arm may not, so check before committing.
        if new_value != value && self.dominates_op(new_value, op_id) {
            self.context.replace_value_uses(value, new_value);
            if let Some((_, operand, result)) = state_edge {
                // A read leaves memory as it found it, so the state it published
                // *is* the state it observed: whoever named the one names the
                // other, joins included.
                self.context.replace_value_uses(result, operand);
            }
            // Only erase a pure value op; an op with regions may have side effects
            // whose result merely became unused (left for DCE).
            if instance.regions().is_empty() {
                rewriter.erase_op(&target)?;
            }
        }
        Ok(())
    }

    /// Whether the def of `value` is in scope at the operation `op` — what a
    /// value an arm yields must be for the terminator that yields it.
    fn dominates_op(&self, value: ValueId, op: OpId) -> bool {
        if scopes::is_module_level(self.context, value) {
            return true;
        }
        let Some(vb) = self.def_block(value) else {
            return false;
        };
        scopes::precedes(
            self.context,
            vb,
            self.context.get_value(value).defining_op(),
            op,
        )
    }

    fn def_block(&self, value: ValueId) -> Option<BlockId> {
        match self.context.get_value(value).defining_op() {
            Some(op) => self.context.get_op(op).parent_block(),
            None => self.arg_block.get(&value).copied(),
        }
    }

    /// Recurse into each nested region, assuming its guard's fact inside it.
    fn recurse(&mut self, op_ids: &[OpId], rewriter: &mut Rewriter) -> Result<(), PassError> {
        for &op_id in op_ids {
            if !self.context.has_operation(op_id) {
                continue;
            }
            let instance = self.context.get_op(op_id);
            if instance.regions().is_empty() {
                continue;
            }
            let guarded = region_facts(&instance);
            for sub in instance.regions() {
                match guarded.iter().find(|&&(r, ..)| r == sub) {
                    Some(&(_, value, holds)) => {
                        self.eg.push_context();
                        self.inject(value, holds);
                        self.process_region(sub, rewriter)?;
                        self.eg.pop_context();
                    }
                    None => self.process_region(sub, rewriter)?,
                }
            }
        }
        Ok(())
    }

    /// Prove the loop-carried values a loop never changes, optimistically.
    ///
    /// SCCP's distinctive power as a scope: hypothesise that a port holds the
    /// constant the loop was entered on, run the body under that hypothesis, and
    /// keep the ports no edge back into them refutes. What survives is a fact
    /// about the base graph, so it is unioned there and every read of the port —
    /// inside the loop and after it — is that constant. What does not survive is
    /// dropped and the round runs again; `hypotheses` only shrinks, so it ends.
    ///
    /// A nest is done under its own enclosing scope: an inner port entered on
    /// what an outer one carries is only constant while the outer hypothesis is
    /// open, and an outer port latched from an inner loop's result is only
    /// unrefuted once the inner one is proved. So the inner loops are resolved
    /// inside the outer scope before the outer round reads its own edges, and
    /// again in the base graph once the outer hypothesis is promoted there.
    fn hypothesize(&mut self, loops: Vec<LoopPorts>) {
        let mut order: Vec<usize> = (0..loops.len()).collect();
        order.sort_by_key(|&index| self.nesting(loops[index].op));
        self.hypothesize_within(&loops, &order, None);
    }

    /// The loops `parent` holds directly, resolved in whatever scope is open.
    fn hypothesize_within(&mut self, loops: &[LoopPorts], order: &[usize], parent: Option<OpId>) {
        for &index in order {
            let holder = &loops[index];
            if self.enclosing_loop(loops, holder.op) != parent {
                continue;
            }
            let mut hypotheses: Vec<&Port> = holder
                .ports
                .iter()
                .filter(|port| self.is_constant(port.init))
                .collect();
            while !hypotheses.is_empty() {
                self.eg.push_context();
                for port in &hypotheses {
                    self.eg.union(port.head, port.init);
                }
                self.eg.rebuild();
                self.eg
                    .saturate(&self.ruleset.rewrites, ITER_LIMIT, NODE_LIMIT);
                self.hypothesize_within(loops, order, Some(holder.op));
                let refuted: Vec<bool> = hypotheses
                    .iter()
                    .map(|port| {
                        port.edges
                            .iter()
                            .any(|&edge| self.eg.find(edge) != self.eg.find(port.init))
                    })
                    .collect();
                self.eg.pop_context();
                if !refuted.contains(&true) {
                    break;
                }
                let mut dropped = refuted.iter();
                hypotheses.retain(|_| !dropped.next().copied().unwrap_or(false));
            }
            for port in hypotheses {
                self.eg.union(port.head, port.init);
                // The loop is left with what its test forwarded, which is the
                // head itself only where the test forwards it unchanged. Under a
                // proven hypothesis the head is the constant everywhere, so the
                // forwarded class says what the result is; the init does not.
                self.eg.union(port.result, port.published);
            }
            self.eg.rebuild();
            self.hypothesize_within(loops, order, Some(holder.op));
        }
    }

    /// The innermost loop of `loops` holding `op`'s own regions.
    fn enclosing_loop(&self, loops: &[LoopPorts], op: OpId) -> Option<OpId> {
        let mut block = self.context.get_op(op).parent_block();
        while let Some(current) = block {
            let region = self.context.parent_region(current)?;
            let holder = self.context.get_region(region).parent_op()?;
            if loops.iter().any(|other| other.op == holder) {
                return Some(holder);
            }
            block = self.context.get_op(holder).parent_block();
        }
        None
    }

    fn is_constant(&self, class: Id) -> bool {
        self.eg.nodes(class).iter().any(|node| node.int().is_some())
    }

    /// How deep in the region tree `op` sits, so the loops are read outermost first.
    fn nesting(&self, op: OpId) -> usize {
        let mut depth = 0;
        let mut block = self.context.get_op(op).parent_block();
        while let Some(current) = block {
            let Some(region) = self.context.parent_region(current) else {
                break;
            };
            depth += 1;
            block = self
                .context
                .get_region(region)
                .parent_op()
                .and_then(|holder| self.context.get_op(holder).parent_block());
        }
        depth
    }

    /// Assume `value == holds` in the current context by unioning its class with the
    /// matching boolean constant. An equality the assumption settles says more than
    /// the truth of the condition: the two operands name one value there, so their
    /// classes are merged as well and every term over either is a term over the
    /// cheapest form of both — the literal, where one side is one.
    fn inject(&mut self, value: ValueId, holds: bool) {
        let cond = self.class_of(value);
        let constant = self
            .eg
            .add(Node::constant(APInt::new(1, holds as u64), Prov::None));
        self.eg.union(cond, constant);
        if let Some((lhs, rhs)) = self.settled_equality(value, holds) {
            self.eg.union(lhs, rhs);
        }
        self.eg.rebuild();
    }

    /// The operand classes a guard proves congruent: an `eq` that holds, or a
    /// `ne` that does not.
    fn settled_equality(&mut self, value: ValueId, holds: bool) -> Option<(Id, Id)> {
        let op = self.context.get_value(value).defining_op()?;
        let instance = self.context.get_op(op);
        if !instance.is::<ops::CmpIOp>() {
            return None;
        }
        let AttributeValue::Str(predicate) = instance.attr("predicate")? else {
            return None;
        };
        let equal = match &*predicate {
            "eq" => holds,
            "ne" => !holds,
            _ => return None,
        };
        let [lhs, rhs] = instance.operands()[..] else {
            return None;
        };
        equal.then(|| (self.class_of(lhs), self.class_of(rhs)))
    }

    /// The class standing for `value`, anchoring it as an opaque leaf if the
    /// seeding named none.
    fn class_of(&mut self, value: ValueId) -> Id {
        self.value_class
            .get(&value)
            .copied()
            .unwrap_or_else(|| self.eg.add(Node::input(value)))
    }

    /// Rebuild the value of `class`'s cheapest node: an existing value is reused, a
    /// constant or rule-introduced op is built before `target`. Memoized per class.
    ///
    /// `None` where the term the extraction chose has no value at `target` — a
    /// class no cost model could spell, a gate whose arms do not answer. The
    /// rewrite is then skipped and the operation it would have replaced stays.
    fn materialize(
        &self,
        extraction: &Extraction<Node>,
        class: Id,
        expected_ty: TypeId,
        target: &OperationRef,
        rewriter: &mut Rewriter,
        memo: &mut HashMap<Id, ValueId>,
    ) -> Result<Option<ValueId>, PassError> {
        let class = self.eg.find(class);
        if let Some(&value) = memo.get(&class) {
            return Ok(Some(value));
        }
        let Some(node) = extraction.node(class) else {
            return Ok(None);
        };
        // Provenance decides how a term becomes IR again: a gate stands for its
        // block-argument value, a seeded op or constant for the op that already
        // computes it, a rule-introduced op for its emitter, and a constant no op
        // holds is built here.
        let value = match (node.sym(), node.prov) {
            (_, Prov::Value(_) | Prov::Op(_)) => {
                let named = self.named_value(node);
                let Some(value) = self.spelled_at(named, class, expected_ty, target, rewriter)?
                else {
                    return Ok(None);
                };
                value
            }
            (_, Prov::Introduced(idx)) => {
                let ty = node.ty.expect("an op node carries its result type");
                let mut operands = Vec::with_capacity(node.children.len());
                for &arg in &node.children {
                    let Some(operand) =
                        self.materialize(extraction, arg, ty, target, rewriter, memo)?
                    else {
                        return Ok(None);
                    };
                    operands.push(operand);
                }
                let emit = self.ruleset.emits[idx]
                    .as_ref()
                    .expect("an introduced op supplies an emit");
                emit(self.context, &operands, ty, target, rewriter)?
            }
            (_, Prov::None) => {
                let Some(literal) = node.int() else {
                    return Ok(None);
                };
                super::literal_before(self.context, rewriter, spell(literal), expected_ty, target)?
            }
        };
        memo.insert(class, value);
        Ok(Some(value))
    }

    /// The value the class takes where `target` sits, `named` being the form the
    /// extraction chose where it still names one. A class is one term, but the
    /// values holding it are spread over the tree: the form a bottom-up extraction
    /// picked may sit in a sibling region — an arm of the very gate a port is being
    /// grown on, say — and name nothing here.
    ///
    /// What a spelling costs where it is read is the number of regions it crosses:
    /// a form defined in the reader's own region crosses none, one from an
    /// enclosing region stays live across every region in between, and a literal —
    /// built at the reader — crosses none either. So the nearest form of the class
    /// that reaches answers, and where the nearest is an outer one a literal the
    /// class carries answers in its place — unless a loop separates the two, where
    /// the outer form is loop-invariant and the literal would be rebuilt every
    /// iteration. Failing every form: the literal the class carries, which is
    /// spelled anywhere and computes nothing; and failing that the value being
    /// rewritten itself, which answers for the class but changes nothing.
    fn spelled_at(
        &self,
        named: Option<ValueId>,
        class: Id,
        ty: TypeId,
        target: &OperationRef,
        rewriter: &mut Rewriter,
    ) -> Result<Option<ValueId>, PassError> {
        let own = self.context.get_op(target.op().id).results().to_vec();
        // The extraction's own choice leads, so it answers where nothing is nearer.
        let mut reaching: Vec<ValueId> = Vec::new();
        if let Some(named) = named.filter(|&named| self.reaches(named, target)) {
            reaching.push(named);
        }
        reaching.extend(
            self.eg
                .nodes(class)
                .iter()
                .filter_map(|node| self.named_value(node))
                .filter(|&value| Some(value) != named && self.reaches(value, target)),
        );
        let nearest = reaching
            .iter()
            .copied()
            .filter(|value| !own.contains(value) && Some(*value) != self.replacing.get())
            .min_by_key(|&value| self.regions_out(value, target));
        if let Some(value) = nearest {
            if self.regions_out(value, target) == 0 || self.crosses_loop(value, target) {
                return Ok(Some(value));
            }
            // An outer form drags a live range through every region in between.
            if let Some(literal) = self.literal_at(class, ty, target, rewriter)? {
                return Ok(Some(literal));
            }
            return Ok(Some(value));
        }
        // A literal is spelled anywhere and computes nothing, so it answers where
        // no form of the class reaches but the one being rewritten. Failing that,
        // that form answers for the class and changes nothing.
        if let Some(literal) = self.literal_at(class, ty, target, rewriter)? {
            return Ok(Some(literal));
        }
        Ok(reaching.first().copied())
    }

    /// The literal `class` carries, built where `target` sits.
    fn literal_at(
        &self,
        class: Id,
        ty: TypeId,
        target: &OperationRef,
        rewriter: &mut Rewriter,
    ) -> Result<Option<ValueId>, PassError> {
        let Some(literal) = self.eg.nodes(class).iter().find_map(Node::int) else {
            return Ok(None);
        };
        Ok(Some(super::literal_before(
            self.context,
            rewriter,
            spell(literal),
            ty,
            target,
        )?))
    }

    /// How many regions out of `target`'s own `value` is defined — 0 where both
    /// sit in the same region, one per enclosing region otherwise.
    fn regions_out(&self, value: ValueId, target: &OperationRef) -> usize {
        let here = self
            .context
            .get_op(target.op().id)
            .parent_block()
            .map(|block| self.region_depth(block));
        let there = self.def_block(value).map(|block| self.region_depth(block));
        match (here, there) {
            (Some(here), Some(there)) => here.saturating_sub(there),
            _ => usize::MAX,
        }
    }

    /// Whether a loop separates where `value` is defined from where `target`
    /// reads it. A gate's arm runs at most as often as the gate, so a literal
    /// built there replaces a live range with an instruction that is not paid
    /// more often; inside a loop it is paid every iteration, where a value from
    /// outside is loop-invariant and sits in a register.
    fn crosses_loop(&self, value: ValueId, target: &OperationRef) -> bool {
        let Some(defined_in) = self.def_block(value).map(|block| self.region_depth(block)) else {
            return true;
        };
        let mut block = self.context.get_op(target.op().id).parent_block();
        while let Some(current) = block.filter(|&block| self.region_depth(block) > defined_in) {
            let Some(holder) = self
                .context
                .parent_region(current)
                .and_then(|region| self.context.get_region(region).parent_op())
            else {
                return false;
            };
            if self
                .context
                .get_op(holder)
                .as_interface::<dyn LoopLike>()
                .is_some()
            {
                return true;
            }
            block = self.context.get_op(holder).parent_block();
        }
        false
    }

    fn region_depth(&self, block: BlockId) -> usize {
        let mut depth = 0;
        let mut current = Some(block);
        while let Some(block) = current {
            let Some(region) = self.context.parent_region(block) else {
                break;
            };
            depth += 1;
            current = self
                .context
                .get_region(region)
                .parent_op()
                .and_then(|holder| self.context.get_op(holder).parent_block());
        }
        depth
    }

    /// The value a node names outright, building nothing.
    fn named_value(&self, node: &Node) -> Option<ValueId> {
        match (node.sym(), node.prov) {
            (_, Prov::Value(value)) => self.context.has_value(value).then_some(value),
            (Some(SymKind::If | SymKind::Theta | SymKind::LoadMemory), Prov::Op(_)) => None,
            (_, Prov::Op(op)) => self
                .context
                .has_operation(op)
                .then(|| self.context.get_op(op).results().first().copied())
                .flatten(),
            _ => None,
        }
    }

    /// Whether `value` is in scope where `target` sits — the operation being
    /// rewritten names its own value, which is what a rewrite replaces.
    fn reaches(&self, value: ValueId, target: &OperationRef) -> bool {
        let op = target.op().id;
        self.context.get_value(value).defining_op() == Some(op) || self.dominates_op(value, op)
    }
}

/// The assumption each of `op`'s regions runs under, read off the operation's own
/// interfaces: a [`Conditional`]'s guarded arm runs on its decision holding, and a
/// tested loop's body runs on the condition its test region yields — which holds on
/// every iteration, since the condition is spelled over the ports' per-iteration
/// heads. A region a structured operation states nothing about (a switch case, a
/// loop's own test) carries no fact.
fn region_facts(op: &crate::OpHandle) -> Vec<(RegionId, ValueId, bool)> {
    if let Some(conditional) = op.clone().as_interface::<dyn Conditional>() {
        return conditional.guarded_regions();
    }
    let Some(guard) = op.clone().as_interface::<dyn GuardedLoop>() else {
        return Vec::new();
    };
    let EntryGuard::Region {
        region: test,
        condition,
        ..
    } = guard.entry_guard()
    else {
        return Vec::new();
    };
    op.regions()
        .iter()
        .filter(|&&region| region != test)
        .map(|&region| (region, condition, true))
        .collect()
}

/// How a literal is written down. A class holds one node per bit pattern however
/// each spelling read it back, so the sign flag on whichever node the class
/// happens to hold says nothing about the value — and a one-bit literal is a
/// truth value, which is `1`, not the `-1` a signed reading of that bit gives.
fn spell(literal: &APInt) -> i64 {
    match literal.width() {
        1 => literal.to_u64() as i64,
        _ => literal.to_i64(),
    }
}
