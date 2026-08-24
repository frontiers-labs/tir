//! InstCombine: an equality-saturation peephole. It seeds the function's regions
//! ([`seed`], which reads gates off the ops' own interfaces) into a
//! [`tir_symbolic`] e-graph of real IR values, saturates, extracts the cheapest
//! form per value by [`crate::OpCost`], and rewrites what
//! improved. Flow-sensitive facts ride the e-graph's scoped assumptions: a
//! structured region pushes its guard's condition around its body, and unstructured
//! `cond_br` facts ([`crate::analysis::DominatingEdgeFacts`]) are asserted in
//! dominator-tree DFS order — each block's own guard fact scoped once and inherited
//! by the blocks it dominates — then popped on the way back up.
//!
//! The engine holds no op-specific knowledge — identity, cost, folding and
//! constant-reading come from op interfaces; op construction is owned by the rewrites.

pub(crate) mod rules;
mod seed;
mod state;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use tir_symbolic::egraph::{EGraph, ENode, Extraction, Id};

use std::rc::Rc;

use crate::analysis::scopes::{self, port_edges};
use crate::analysis::{DominatingEdgeFacts, DominatorTree};
use crate::graph::Dag;

use crate::{
    AnalysisManager, BlockId, Conditional, ConstantLike, Context, EntryGuard, GuardedLoop,
    LoopLike, MemoryRead, OpHandle, OpId, OpInstance, OperationRef, Pass, PassError, PassTarget,
    RegionId, Rewriter, TokenScope, TypeId, ValueId, builtin::StateType, func::FuncOp,
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
        analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        if op.as_op::<FuncOp>().is_none() {
            return Ok(());
        }
        let root = op.op().id;
        let seeded = seed::seed(analyses, context, root);
        let ruleset = builtin_ruleset(context, &seeded);
        let mut driver = Driver {
            context,
            eg: seeded.eg,
            value_class: seeded.value_class,
            arg_block: RefCell::new(seeded.arg_block),
            dom: analyses.get::<DominatorTree>(context, root),
            edge_facts: analyses.get::<DominatingEdgeFacts>(context, root),
            ruleset,
            gamma_ports: RefCell::new(HashMap::new()),
            theta_ports: RefCell::new(Vec::new()),
            port_bindings: RefCell::new(Vec::new()),
            growing: RefCell::new(Vec::new()),
            rereads: RefCell::new(HashMap::new()),
        };
        let body = context.get_op(root).regions()[0];
        driver.process_region(body, rewriter)?;
        // Rewrites erase and insert ops within blocks but never touch the block
        // graph, so dominance survives; the value graph does not.
        Ok(())
    }
}

/// Rewrites each region under the assumptions that hold there, and *before* its
/// children's scopes open so the base classes a child scope reads are final.
struct Driver<'a> {
    context: &'a Context,
    eg: EGraph<Node>,
    value_class: HashMap<ValueId, Id>,
    /// The block each block argument belongs to. A commit growing a carried port
    /// adds arguments of its own, which the dominance check has to locate too.
    arg_block: RefCell<HashMap<ValueId, BlockId>>,
    dom: Rc<DominatorTree>,
    edge_facts: Rc<DominatingEdgeFacts>,
    ruleset: Ruleset,
    /// The value port already grown on a gate for its arms' classes, so every
    /// read the gate answers takes the one port.
    gamma_ports: RefCell<HashMap<(OpId, Vec<Id>), ValueId>>,
    /// The same for a loop, over the classes its port is fed from. A list, not a
    /// map: a read outside the loop asks for the class the port is left holding,
    /// which is not the key, so every port is scanned in the order they were grown.
    /// A port under construction is listed without one: growing it may ask for a
    /// value it carries itself, and a port answers nothing until it is fed.
    theta_ports: RefCell<Vec<(ThetaKey, Option<ThetaPort>)>>,
    /// The θ classes standing for a port argument while that port's latch is
    /// being built: inside the body the carry *is* the argument.
    port_bindings: RefCell<Vec<(Id, ValueId)>>,
    /// The gates whose ports are being grown, innermost last.
    growing: RefCell<Vec<OpId>>,
    /// The copy already spliced in for a read class, so every arm a law
    /// distributed that read into takes the one copy.
    rereads: RefCell<HashMap<Id, ValueId>>,
}

/// The loop a θ port belongs to and the classes its edges are fed from.
type ThetaKey = (OpId, Vec<Id>);

/// A carried port a θ commit grew: the argument each of the loop's regions reads
/// the carried value as, and the result the loop leaves it in — each with the
/// class it stands for there. A port holds one class per place, and not the same
/// one everywhere: a loop that latches in its test is entered on the head and
/// left with the latch.
struct ThetaPort {
    arguments: Vec<(RegionId, Id, ValueId)>,
    result: (Id, ValueId),
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

        let blocks: Vec<BlockId> = self
            .context
            .get_region(region)
            .iter(self.context.clone())
            .map(|block| block.id())
            .collect();
        let region_blocks: HashSet<BlockId> = blocks.iter().copied().collect();

        // Rewrite blocks in dominator-tree DFS order so each block's own guard
        // fact, pushed as a scope, is inherited by the blocks it dominates. The
        // region entry dominates the rest of the region, so it roots the DFS.
        let mut visited = HashSet::new();
        if let Some(&entry) = blocks.first() {
            self.rewrite_block_tree(entry, &region_blocks, &extraction, rewriter, &mut visited)?;
        }
        // Blocks unreachable in the dominator tree carry no inherited fact.
        for &block in &blocks {
            if visited.insert(block) {
                for op_id in self.context.get_block(block).op_ids() {
                    self.rewrite_op(op_id, &extraction, rewriter)?;
                }
            }
        }

        let op_ids: Vec<OpId> = blocks
            .iter()
            .flat_map(|&block| self.context.get_block(block).op_ids())
            .collect();
        self.recurse(&op_ids, rewriter)
    }

    /// Rewrite `block` then its dominator subtree (restricted to this region).
    /// A block's own guard fact holds throughout that subtree, so inject it once
    /// under a fresh scope and let the nesting carry it down — no re-assertion.
    fn rewrite_block_tree(
        &mut self,
        block: BlockId,
        region_blocks: &HashSet<BlockId>,
        parent_extraction: &Extraction<Node>,
        rewriter: &mut Rewriter,
        visited: &mut HashSet<BlockId>,
    ) -> Result<(), PassError> {
        if !visited.insert(block) {
            return Ok(());
        }
        // A condition without a seeded class cannot be assumed; skip, don't panic.
        let pushed = match self.edge_facts.own_fact(block) {
            Some(fact) if self.value_class.contains_key(&fact.condition) => {
                self.eg.push_context();
                self.inject(fact.condition, fact.holds);
                self.eg
                    .saturate(&self.ruleset.rewrites, ITER_LIMIT, NODE_LIMIT);
                true
            }
            _ => false,
        };
        // A pushed fact changes what extracts cheapest; reuse the parent's otherwise.
        let local = pushed.then(|| self.eg.extract_best(|_, node| cost(node)));
        let extraction = local.as_ref().unwrap_or(parent_extraction);

        for op_id in self.context.get_block(block).op_ids() {
            self.rewrite_op(op_id, extraction, rewriter)?;
        }

        if let Some(node) = self.dom.node_of(block) {
            let children: Vec<BlockId> = self
                .dom
                .children(node)
                .filter_map(|child| self.dom.block(child))
                .filter(|child| region_blocks.contains(child))
                .collect();
            for child in children {
                self.rewrite_block_tree(child, region_blocks, extraction, rewriter, visited)?;
            }
        }

        if pushed {
            self.eg.pop_context();
        }
        Ok(())
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
            _ => return Ok(()),
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
        // A state chain is linear, so handing this op's readers another state is
        // only sound when the op is erased and the state it consumed is the very
        // one handed on — its use then moves rather than doubles. A gate keeping
        // its regions, or a chain named anywhere else, would be consumed twice.
        if ty == StateType::new(self.context)
            && !(instance.regions().is_empty() && instance.operands().contains(&new_value))
        {
            return Ok(());
        }
        // The replacement must dominate the use it takes over. Operand reuse and
        // freshly built ops satisfy this by construction; a cross-block CSE or a gate
        // collapsing to an arm may not, so check before committing.
        if new_value != value && self.dominates(new_value, value) {
            self.context.replace_value_uses(value, new_value);
            if let Some((_, operand, result)) = state_edge {
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

    /// Whether the def of `a` dominates the def of `b`. `b` is always an op result,
    /// so it has a defining op; `a` may be a block argument, located via `arg_block`.
    fn dominates(&self, a: ValueId, b: ValueId) -> bool {
        if scopes::is_module_level(self.context, a) {
            return true;
        }
        let (Some(ab), Some(bb)) = (self.def_block(a), self.def_block(b)) else {
            return false;
        };
        if ab != bb {
            return self.dom.dominates(ab, bb) && scopes::reaches_into(self.context, a, ab, bb);
        }
        match (
            self.context.get_value(a).defining_op(),
            self.context.get_value(b).defining_op(),
        ) {
            (Some(a_op), Some(b_op)) => self.context.get_block(ab).is_before(a_op, b_op),
            // A block argument precedes every op in its block.
            (None, _) => true,
            (Some(_), None) => false,
        }
    }

    /// Whether the def of `value` dominates the operation `op` — what a value an
    /// arm yields must do for the terminator that yields it.
    fn dominates_op(&self, value: ValueId, op: OpId) -> bool {
        if scopes::is_module_level(self.context, value) {
            return true;
        }
        let Some(vb) = self.def_block(value) else {
            return false;
        };
        scopes::precedes(
            self.context,
            &self.dom,
            vb,
            self.context.get_value(value).defining_op(),
            op,
        )
    }

    /// Whether what `op` defines is in scope where `target` sits — what a port
    /// grown on a gate is worth to a read outside it.
    fn op_reaches(&self, op: OpId, target: &OperationRef) -> bool {
        let Some(block) = self.context.get_op(op).parent_block() else {
            return false;
        };
        scopes::precedes(self.context, &self.dom, block, Some(op), target.op().id)
    }

    fn def_block(&self, value: ValueId) -> Option<BlockId> {
        match self.context.get_value(value).defining_op() {
            Some(op) => self.context.get_op(op).parent_block(),
            None => self.arg_block.borrow().get(&value).copied(),
        }
    }

    /// Recurse into each nested region, assuming a guard's fact inside its region.
    fn recurse(&mut self, op_ids: &[OpId], rewriter: &mut Rewriter) -> Result<(), PassError> {
        for &op_id in op_ids {
            if !self.context.has_operation(op_id) {
                continue;
            }
            let instance = self.context.get_op(op_id);
            if instance.regions().is_empty() {
                continue;
            }
            let guarded = instance
                .clone()
                .as_interface::<dyn Conditional>()
                .map(|g| g.guarded_regions())
                .unwrap_or_default();
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

    /// Assume `value == holds` in the current context by unioning its class with the
    /// matching boolean constant.
    fn inject(&mut self, value: ValueId, holds: bool) {
        let cond = self
            .value_class
            .get(&value)
            .copied()
            .unwrap_or_else(|| self.eg.add(Node::input(value)));
        let constant = self
            .eg
            .add(Node::constant(APInt::new(1, holds as u64), Prov::None));
        self.eg.union(cond, constant);
        self.eg.rebuild();
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
        if let Some(value) = self.bound_port(class, target) {
            return Ok(Some(value));
        }
        let Some(node) = self.chosen(extraction, class) else {
            return Ok(None);
        };
        // Provenance decides how a term becomes IR again: a gate stands for its
        // block-argument value, a seeded op or constant for the op that already
        // computes it, a rule-introduced op for its emitter, and a constant no op
        // holds is built here. A law-introduced gate or access names the operation
        // that rebuilds it: the gate grows a port, the access is copied.
        let value = match (node.sym(), node.prov) {
            (Some(SymKind::If), Prov::Op(gate)) => {
                // A gate inside a loop the reader has left answers through the
                // loop, not through a port of its own.
                let held =
                    match self.latched_result(extraction, class, expected_ty, target, rewriter)? {
                        Some(value) => Some(value),
                        None => {
                            self.commit_gamma_port(extraction, gate, node, expected_ty, rewriter)?
                        }
                    };
                // A port on the gate the extraction chose names the value where
                // that gate is in scope and nowhere else, so a read outside it
                // takes the form of a gate further out.
                let value = match held {
                    Some(value) if self.reaches(value, target) => Some(value),
                    held => self
                        .gamma_port(extraction, class, expected_ty, target, rewriter)?
                        .or(held),
                };
                let Some(value) = value else {
                    return Ok(None);
                };
                value
            }
            (Some(SymKind::Theta), Prov::Op(_)) => {
                let Some(value) =
                    self.commit_theta_port(extraction, class, node, expected_ty, target, rewriter)?
                else {
                    return Ok(None);
                };
                value
            }
            (Some(SymKind::LoadMemory), Prov::Op(load)) => {
                let Some(value) = self.reread(extraction, class, load, node, target, rewriter)?
                else {
                    return Ok(None);
                };
                value
            }
            (_, Prov::Value(_) | Prov::Op(_)) => {
                let named = self.named_value(node);
                let Some(value) =
                    self.spelled_at(extraction, named, class, expected_ty, target, rewriter)?
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
                super::literal_before(
                    self.context,
                    rewriter,
                    literal.to_i64(),
                    expected_ty,
                    target,
                )?
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
    /// iteration. Failing every form: a port carrying the
    /// class out of the region it is computed in; failing that the value being
    /// rewritten itself, which answers for the class but changes nothing; and
    /// failing that the literal, which is spelled anywhere.
    fn spelled_at(
        &self,
        extraction: &Extraction<Node>,
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
            .filter(|value| !own.contains(value))
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
        if let Some(value) = self.carried_out(extraction, class, ty, target, rewriter)? {
            return Ok(Some(value));
        }
        if let Some(&value) = reaching.first() {
            return Ok(Some(value));
        }
        self.literal_at(class, ty, target, rewriter)
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
            literal.to_i64(),
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

    /// The class carried out of the region it is computed in, on a port.
    ///
    /// Which port a class needs is a matter of where it is being read: an arm's
    /// own value stands for the whole gate under the arm's assumption and answers
    /// for it nowhere else, and a loop that latches in its test leaves a class no
    /// θ of its own names. Both are grown at the read that asks — and neither
    /// answers a read it holds: a port carries a value out of a region, so asking
    /// it for one inside that region asks it to carry what it is being fed.
    fn carried_out(
        &self,
        extraction: &Extraction<Node>,
        class: Id,
        ty: TypeId,
        target: &OperationRef,
        rewriter: &mut Rewriter,
    ) -> Result<Option<ValueId>, PassError> {
        if let Some(value) = self.gamma_port(extraction, class, ty, target, rewriter)? {
            return Ok(Some(value));
        }
        self.latched_result(extraction, class, ty, target, rewriter)
    }

    /// The class carried out on a port of the gate that answers where `target`
    /// sits. A class written under nested gates names a gate form per gate it is
    /// written under, and only the form whose gate is in scope at the read carries
    /// the value all the way out — the inner ones name it where the reader cannot
    /// see it. So the gates are tried in the order the graph holds them and the
    /// first whose port reaches answers, which grows a port on every gate in
    /// between as each arm materializes the form one level down.
    fn gamma_port(
        &self,
        extraction: &Extraction<Node>,
        class: Id,
        ty: TypeId,
        target: &OperationRef,
        rewriter: &mut Rewriter,
    ) -> Result<Option<ValueId>, PassError> {
        for node in self.eg.nodes(class) {
            let (Some(SymKind::If), Prov::Op(gate)) = (node.sym(), node.prov) else {
                continue;
            };
            if !self.answers(gate, target) || !self.op_reaches(gate, target) {
                continue;
            }
            if let Some(value) = self.commit_gamma_port(extraction, gate, node, ty, rewriter)?
                && self.reaches(value, target)
            {
                return Ok(Some(value));
            }
        }
        Ok(None)
    }

    /// The loop result a class the loop is left holding takes outside it.
    ///
    /// A loop that latches in its test leaves what the test forwarded, and that
    /// class names no θ of its own — the θ carrying it names it as an edge. So a
    /// class that answers nowhere the reader sits asks the loops carrying it, and
    /// one the reader is not inside answers with its result. Committing the port
    /// is what makes the class spellable at all, so it happens at the read.
    fn latched_result(
        &self,
        extraction: &Extraction<Node>,
        class: Id,
        ty: TypeId,
        target: &OperationRef,
        rewriter: &mut Rewriter,
    ) -> Result<Option<ValueId>, PassError> {
        let class = self.eg.find(class);
        let key = Node::sym_pattern(SymKind::Theta, Vec::new()).op_key();
        for holder in self.eg.classes_with_op(key) {
            for node in self.eg.nodes(holder) {
                let (Some(SymKind::Theta), Prov::Op(loop_op)) = (node.sym(), node.prov) else {
                    continue;
                };
                let latches = node
                    .children
                    .get(1..)
                    .is_some_and(|edges| edges.iter().any(|&edge| self.eg.find(edge) == class));
                if !latches || self.encloses(loop_op, target) {
                    continue;
                }
                self.commit_theta_port(extraction, holder, node, ty, target, rewriter)?;
                if let Some(value) = self.bound_port(class, target) {
                    return Ok(Some(value));
                }
            }
        }
        Ok(None)
    }

    /// Whether a port on `op` could carry a value out to where `target` sits: not
    /// one being grown — it would be asked to carry what it is being fed — and not
    /// one `target` sits inside, where its result is out of scope anyway.
    fn answers(&self, op: OpId, target: &OperationRef) -> bool {
        !self.growing.borrow().contains(&op) && !self.encloses(op, target)
    }

    /// Whether `target` sits inside one of `op`'s regions.
    fn encloses(&self, op: OpId, target: &OperationRef) -> bool {
        if !self.context.has_operation(op) {
            return false;
        }
        let regions = self.context.get_op(op).regions().to_vec();
        let mut block = self.context.get_op(target.op().id).parent_block();
        while let Some(current) = block {
            let Some(region) = self.context.parent_region(current) else {
                return false;
            };
            if regions.contains(&region) {
                return true;
            }
            block = self
                .context
                .get_region(region)
                .parent_op()
                .and_then(|holder| self.context.get_op(holder).parent_block());
        }
        false
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

    /// Carry a law-introduced gate's value out of `gate` on a port of its own.
    ///
    /// Each arm yields the value of the class the gate chose there, materialized
    /// at that arm's terminator, so the port is wired the one way
    /// [`Context::grow_port`] keeps results, arguments and yields consistent. An
    /// arm that cannot answer — no value in scope there — leaves the gate as it
    /// was. One port per gate and pair of arm classes, however many reads it
    /// answers.
    fn commit_gamma_port(
        &self,
        extraction: &Extraction<Node>,
        gate: OpId,
        node: &Node,
        ty: TypeId,
        rewriter: &mut Rewriter,
    ) -> Result<Option<ValueId>, PassError> {
        self.growing.borrow_mut().push(gate);
        let grown = self.grow_gamma_port(extraction, gate, node, ty, rewriter);
        self.growing.borrow_mut().pop();
        grown
    }

    fn grow_gamma_port(
        &self,
        extraction: &Extraction<Node>,
        gate: OpId,
        node: &Node,
        ty: TypeId,
        rewriter: &mut Rewriter,
    ) -> Result<Option<ValueId>, PassError> {
        let [_, arms @ ..] = &node.children[..] else {
            return Ok(None);
        };
        let arms: Vec<Id> = arms.iter().map(|&arm| self.eg.find(arm)).collect();
        let key = (gate, arms.clone());
        if let Some(value) = self.gamma_ports.borrow().get(&key) {
            return Ok(Some(*value));
        }
        if !self.context.has_operation(gate) {
            return Ok(None);
        }
        let instance = self.context.get_op(gate);
        let Some(conditional) = instance.clone().as_interface::<dyn Conditional>() else {
            return Ok(None);
        };
        let cases = conditional.case_values();
        if cases.len() != arms.len() {
            return Ok(None);
        }
        let mut yielded: Vec<(RegionId, ValueId)> = Vec::new();
        for (&(region, _), &class) in cases.iter().zip(&arms) {
            let Some(terminator) = self.terminator(region) else {
                return Ok(None);
            };
            let target = self.at(terminator);
            let mut memo = HashMap::new();
            let Some(value) =
                self.materialize(extraction, class, ty, &target, rewriter, &mut memo)?
            else {
                return Ok(None);
            };
            if !self.dominates_op(value, terminator) {
                return Ok(None);
            }
            yielded.push((region, value));
        }
        let result = self.context.grow_port(gate, ty, None, |region, _| {
            yielded
                .iter()
                .find(|&&(carried, _)| carried == region)
                .map(|&(_, value)| value)
        });
        self.gamma_ports.borrow_mut().insert(key, result);
        Ok(Some(result))
    }

    /// The term to rebuild `class` from: the cheapest form the cost model found,
    /// unless a law answered the class with a θ.
    ///
    /// A θ's latch is a term over the port itself, so its cost is a fixpoint over
    /// the very class being extracted and no bottom-up model can prefer it,
    /// however cheap the port is. In the IR that self-reference is a block
    /// argument and costs nothing, so a θ a law introduced is the answer.
    fn chosen<'a>(&'a self, extraction: &'a Extraction<Node>, class: Id) -> Option<&'a Node> {
        self.eg
            .nodes(class)
            .iter()
            .find(|node| node.sym() == Some(SymKind::Theta) && matches!(node.prov, Prov::Op(_)))
            .or_else(|| extraction.node(class))
    }

    /// The value a port already carries `class` on where `target` sits: the one
    /// bound while a port's own edges are being fed, or one a committed port
    /// answers with.
    fn bound_port(&self, class: Id, target: &OperationRef) -> Option<ValueId> {
        let class = self.eg.find(class);
        let bound = self
            .port_bindings
            .borrow()
            .iter()
            .rev()
            .find(|&&(bound, _)| self.eg.find(bound) == class)
            .map(|&(_, value)| value);
        // Nested loops each carry the class on a port of their own, and the one
        // the read sits innermost inside is the one holding it there — not
        // whichever was grown first.
        bound.or_else(|| {
            self.theta_ports
                .borrow()
                .iter()
                .filter_map(|(_, port)| port.as_ref())
                .filter_map(|port| self.port_value(port, class, target))
                .min_by_key(|&(regions_out, _)| regions_out)
                .map(|(_, value)| value)
        })
    }

    /// Carry a law-introduced θ's value on a port of `loop_op`'s own.
    ///
    /// The port starts at what the class the loop was entered with materializes
    /// to ahead of the loop; every region of the loop reads it as one more
    /// argument, which a region that only tests the carried values forwards on;
    /// and every edge back into the port — the body's latch, and each
    /// `break`/`continue` leaving its scope — carries what its own class
    /// materializes to right there, under a binding resolving the θ itself to
    /// that argument: inside the body the carry *is* the argument. What the port
    /// is worth at `target` is then the argument of the region holding it, or the
    /// loop's result outside them all.
    fn commit_theta_port(
        &self,
        extraction: &Extraction<Node>,
        class: Id,
        node: &Node,
        ty: TypeId,
        target: &OperationRef,
        rewriter: &mut Rewriter,
    ) -> Result<Option<ValueId>, PassError> {
        let (Prov::Op(loop_op), [init_class, edge_classes @ ..]) = (node.prov, &node.children[..])
        else {
            return Ok(None);
        };
        let key: ThetaKey = (
            loop_op,
            node.children.iter().map(|&id| self.eg.find(id)).collect(),
        );
        if self
            .theta_ports
            .borrow()
            .iter()
            .any(|(known, _)| *known == key)
        {
            return Ok(self.bound_port(class, target));
        }
        if !self.context.has_operation(loop_op) {
            return Ok(None);
        }
        self.theta_ports.borrow_mut().push((key.clone(), None));
        let instance = self.context.get_op(loop_op);
        let Some(scope) = instance.clone().as_interface::<dyn TokenScope>() else {
            return Ok(None);
        };
        let body = scope.token_scope_regions();
        let Some(&body_region) = body.first() else {
            return Ok(None);
        };
        let edges = port_edges(self.context, body_region);
        if edges.len() != edge_classes.len() {
            return Ok(None);
        }
        // A loop whose body computes nothing latches in its test: what the test
        // forwards is what the next iteration is entered on, and what the loop is
        // left with. Restructuring leaves every `goto`/`while` loop in that shape.
        let test = match (self.tests_the_latch(&instance, body_region), edge_classes) {
            (Some(region), [_]) => self.terminator(region).map(|edge| (region, edge)),
            _ => None,
        };
        let head = self.eg.find(class);
        let carried_class = match test {
            Some(_) => self.eg.find(edge_classes[0]),
            None => head,
        };
        let at_loop = self.at(loop_op);
        let mut memo = HashMap::new();
        let Some(init) =
            self.materialize(extraction, *init_class, ty, &at_loop, rewriter, &mut memo)?
        else {
            return Ok(None);
        };
        if !self.dominates_op(init, loop_op) {
            return Ok(None);
        }
        let mut arguments = Vec::new();
        let (mut carried, mut tested) = (init, init);
        let result = self
            .context
            .grow_port(loop_op, ty, Some(init), |region, incoming| {
                let incoming = incoming?;
                if let Some(entry) = self.context.get_region(region).block_ids().first() {
                    self.arg_block.borrow_mut().insert(incoming, *entry);
                }
                // The body's edges are fed one by one below — a `break` inside it
                // is one of them, and its terminator may be another. Every other
                // region, a loop's test, forwards the carry on, unless the loop
                // latches there: that operand is spelled below like an edge.
                if body.contains(&region) {
                    carried = incoming;
                    arguments.push((region, carried_class, incoming));
                    return None;
                }
                arguments.push((region, head, incoming));
                if test.is_some_and(|(latching, _)| latching == region) {
                    tested = incoming;
                    return None;
                }
                Some(incoming)
            });
        // Reserve each edge's operand before any value is spelled: feeding one edge
        // may ask for a value another port carries, growing that port and feeding
        // its edges first. Places taken now follow the order the loop carries the
        // ports; places taken as values become known would follow the order they
        // were spelled in, which crosses the ports of a loop that swaps two slots.
        let reserved_test = test.map(|(_, edge)| {
            (
                edge,
                carried_class,
                self.context.append_port_operand(edge, tested),
            )
        });
        let reserved: Vec<(OpId, Id, usize)> = edges
            .into_iter()
            .zip(edge_classes.iter().copied())
            .map(|(edge, edge_class)| {
                let index = self.context.append_port_operand(edge, carried);
                (edge, edge_class, index)
            })
            .collect();
        // The test is fed on the head — it runs before the body — and the body's
        // edges on what the test forwarded them.
        let (tested_all, failure) = self.feed_edges(
            extraction,
            reserved_test.into_iter().collect(),
            ty,
            (head, tested),
            rewriter,
        );
        if let Some(error) = failure {
            return Err(error);
        }
        let (carried_all, failure) =
            self.feed_edges(extraction, reserved, ty, (carried_class, carried), rewriter);
        if let Some(error) = failure {
            return Err(error);
        }
        if !(tested_all && carried_all) {
            return Ok(None);
        }
        let port = ThetaPort {
            arguments,
            result: (carried_class, result),
        };
        let value = self
            .port_value(&port, class, target)
            .map(|(_, value)| value);
        if let Some((_, place)) = self
            .theta_ports
            .borrow_mut()
            .iter_mut()
            .find(|(known, _)| *known == key)
        {
            *place = Some(port);
        }
        Ok(value)
    }

    /// Carry the port's value on each of a loop's edges, each with the class the θ
    /// names it by and the place reserved for it there, under the `bound` reading
    /// of what the port is worth where they are taken. Every edge must be fed —
    /// the loop carries the port whether or not the value can be spelled there — so
    /// an edge whose class does not materialize keeps the port's own argument, which
    /// is what its place was reserved with, and the whole port is reported unfed: no
    /// edit can be taken back, but an unread port answers no reads.
    fn feed_edges(
        &self,
        extraction: &Extraction<Node>,
        edges: Vec<(OpId, Id, usize)>,
        ty: TypeId,
        bound: (Id, ValueId),
        rewriter: &mut Rewriter,
    ) -> (bool, Option<PassError>) {
        let mut fed = true;
        let mut failure = None;
        for (edge, edge_class, index) in edges {
            let at_edge = self.at(edge);
            self.port_bindings.borrow_mut().push(bound);
            let value = self.materialize(
                extraction,
                edge_class,
                ty,
                &at_edge,
                rewriter,
                &mut HashMap::new(),
            );
            self.port_bindings.borrow_mut().pop();
            match value {
                Ok(Some(value)) if self.dominates_op(value, edge) => {
                    self.context.set_op_operand(edge, index, value);
                }
                other => {
                    failure = failure.or(other.err());
                    fed = false;
                }
            }
        }
        (fed, failure)
    }

    /// What `port` is worth for `class` where `target` sits, and how many regions
    /// out it answers from: the argument of the innermost loop region holding it,
    /// or the loop's result outside every one of them. `None` where the port holds
    /// another class there — the value it does carry answers for a different point
    /// of the loop.
    fn port_value(
        &self,
        port: &ThetaPort,
        class: Id,
        target: &OperationRef,
    ) -> Option<(usize, ValueId)> {
        let class = self.eg.find(class);
        let mut block = self.context.get_op(target.op().id).parent_block();
        let mut regions_out = 0;
        while let Some(current) = block {
            let Some(region) = self.context.parent_region(current) else {
                break;
            };
            let mut held = port
                .arguments
                .iter()
                .filter(|&&(holding, ..)| holding == region)
                .peekable();
            if held.peek().is_some() {
                return held
                    .find(|&&(_, carried, _)| self.eg.find(carried) == class)
                    .map(|&(.., value)| (regions_out, value));
            }
            regions_out += 1;
            block = self
                .context
                .get_region(region)
                .parent_op()
                .and_then(|op| self.context.get_op(op).parent_block());
        }
        (self.eg.find(port.result.0) == class).then_some((usize::MAX, port.result.1))
    }

    /// A read a law distributed into an arm that stored nothing: with no value
    /// defined there and no operation naming an indeterminate one, the read
    /// itself is the value — a copy of the load being distributed, on the state
    /// the law says it observes.
    ///
    /// The copy splices into the linear chain where that state is *published*,
    /// and what it publishes takes the state's place. Reading it at the consumer
    /// instead would answer the same value — a class is one state however far
    /// along the chain it is named — but would leave every write between the two
    /// points with a reader, and a slot nothing wrote would keep its stores.
    fn reread(
        &self,
        extraction: &Extraction<Node>,
        class: Id,
        load: OpId,
        node: &Node,
        target: &OperationRef,
        rewriter: &mut Rewriter,
    ) -> Result<Option<ValueId>, PassError> {
        if !self.context.has_operation(load) {
            return Ok(None);
        }
        if let Some(&value) = self.rereads.borrow().get(&self.eg.find(class))
            && self.reaches(value, target)
        {
            return Ok(Some(value));
        }
        let template = self.context.get_op(load);
        let Some(read) = template.clone().as_interface::<dyn MemoryRead>() else {
            return Ok(None);
        };
        let Some(observed) = read.state_operand() else {
            return Ok(None);
        };
        let Some((state, cursor)) = self.published_state(node.children[state::LOAD_STATE], target)
        else {
            return Ok(None);
        };
        let mut memo = HashMap::new();
        let location = read.read_location();
        let address_ty = self.context.get_value(location).ty();
        let Some(address) = self.materialize(
            extraction,
            node.children[state::ADDRESS],
            address_ty,
            &cursor,
            rewriter,
            &mut memo,
        )?
        else {
            return Ok(None);
        };
        let results: Vec<ValueId> = template
            .results()
            .iter()
            .map(|&result| {
                self.context
                    .create_value(self.context.get_value(result).ty(), None)
                    .id()
            })
            .collect();
        if let Some(published) = read.state_result()
            && let Some(index) = template.results().iter().position(|&r| r == published)
        {
            self.context.replace_value_uses(state, results[index]);
        }
        let operands = template
            .operands()
            .iter()
            .map(|&operand| match operand {
                _ if operand == location => address,
                _ if operand == observed => state,
                _ => operand,
            })
            .collect();
        let copy = self.context.add_operation(OpInstance::new_dynamic(
            (template.dialect().as_str(), template.name().as_str()),
            self.context.as_context_ref(),
            operands,
            results.clone(),
            vec![],
            template.attributes().to_vec(),
        ));
        rewriter.insert_op_before(&cursor, copy.as_dyn_op().as_ref())?;
        self.rereads
            .borrow_mut()
            .insert(self.eg.find(class), results[0]);
        Ok(Some(results[0]))
    }

    /// The value publishing `class` and a cursor just after it — where the chain
    /// took that state on. `None` where nothing the class names is in scope at
    /// `target`; the earliest publication wins, so a copy built there observes
    /// as little as the class allows.
    fn published_state(&self, class: Id, target: &OperationRef) -> Option<(ValueId, OperationRef)> {
        let mut published: Option<ValueId> = None;
        for node in self.eg.nodes(class) {
            let Some(value) = self.named_value(node) else {
                continue;
            };
            if !self.dominates_op(value, target.op().id) {
                continue;
            }
            if published.is_none_or(|held| self.dominates(value, held)) {
                published = Some(value);
            }
        }
        let value = published?;
        let block = self.def_block(value)?;
        let ops = self.context.get_block(block).op_ids();
        let index = match self.context.get_value(value).defining_op() {
            Some(defining) => ops.iter().position(|&op| op == defining)? + 1,
            // A block argument is published before every operation in its block.
            None => 0,
        };
        Some((value, self.at(*ops.get(index)?)))
    }

    /// A cursor at `op`, for a rewrite to build in front of.
    fn at(&self, op: OpId) -> OperationRef {
        let instance = self.context.get_op(op);
        let block = instance.parent_block().map(|id| self.context.get_block(id));
        OperationRef::new(instance, block, None)
    }

    /// The region a loop tests before each iteration, when its body computes
    /// nothing at all: everything such a loop does happens before the test's
    /// terminator, so the value the test forwards is the latch itself and the
    /// body hands it straight back. Restructuring leaves `goto`/`while` loops in
    /// that shape.
    fn tests_the_latch(&self, instance: &OpHandle, body: RegionId) -> Option<RegionId> {
        let EntryGuard::Region { region, .. } = instance
            .clone()
            .as_interface::<dyn GuardedLoop>()?
            .entry_guard()
        else {
            return None;
        };
        let block = self
            .context
            .get_region(body)
            .iter(self.context.clone())
            .next()?;
        (block.op_ids().len() == 1).then_some(region)
    }

    /// The terminator of `region`, when it is the single-block region a port can
    /// be grown on.
    fn terminator(&self, region: RegionId) -> Option<OpId> {
        let mut blocks = self.context.get_region(region).iter(self.context.clone());
        let block = blocks.next()?;
        if blocks.next().is_some() {
            return None;
        }
        block.op_ids().last().copied()
    }
}
