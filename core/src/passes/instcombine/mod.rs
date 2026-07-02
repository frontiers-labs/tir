//! InstCombine: an equality-saturation peephole. It seeds the function's value graph
//! ([`crate::analysis::GSA`], which already encodes phis as γ/μ/Φ gates so the pass
//! needs no CFG walk) into a [`tir_symbolic`] e-graph of real IR values, saturates,
//! extracts the cheapest form per value by [`crate::OpCost`], and rewrites what
//! improved. Flow-sensitive facts ride the e-graph's scoped assumptions: the region
//! driver pushes a context, assumes a guard's condition, rewrites, and pops.
//!
//! The engine holds no op-specific knowledge — identity, cost, folding and
//! constant-reading come from op interfaces; op construction is owned by the rewrites.

mod node;
mod rules;
mod seed;

use std::collections::HashMap;

use tir_symbolic::egraph::{EGraph, Extraction, Id};

use std::rc::Rc;

use crate::analysis::{DominatorTree, GSA, GateNode};
use crate::{
    AnalysisManager, BlockId, ConstantLike, Context, OpId, Operation, OperationRef, Pass,
    PassError, PassTarget, PreservedAnalyses, RegionGuard, RegionId, Rewriter, TypeId, ValueId,
    builtin::{FuncOp, ops},
    utils::APInt,
};

use node::{Node, OpProv};
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
        PassTarget::Operation(FuncOp::name())
    }

    fn run(
        &mut self,
        op: &OperationRef,
        context: &Context,
        rewriter: &mut Rewriter,
        analyses: &AnalysisManager,
    ) -> Result<PreservedAnalyses, PassError> {
        if op.as_op::<FuncOp>().is_none() {
            return Ok(PreservedAnalyses::all());
        }
        let root = op.op().id;
        let seeded = seed::seed(context, root, &analyses.get::<GSA>(context, root));
        let mut driver = Driver {
            context,
            eg: seeded.eg,
            value_class: seeded.value_class,
            arg_block: seeded.arg_block,
            dom: analyses.get::<DominatorTree>(context, root),
            ruleset: builtin_ruleset(context),
        };
        let body = context.get_op(root).regions[0];
        driver.process_region(body, rewriter)?;
        // Rewrites erase and insert ops within blocks but never touch the block
        // graph, so dominance survives; the value graph does not.
        Ok(PreservedAnalyses::none().preserve::<DominatorTree>())
    }
}

/// The block-argument value a gate stands for at write-back.
fn gate_value(gate: &GateNode) -> ValueId {
    match gate {
        GateNode::Input(v) => *v,
        GateNode::Gamma { value, .. } | GateNode::Mu { value } | GateNode::Phi { value } => *value,
        GateNode::Op(_) => unreachable!("an op is a Node::Op, never a Node::Gate"),
    }
}

/// Rewrites each region under the assumptions that hold there, and *before* its
/// children's scopes open so the base classes a child scope reads are final.
struct Driver<'a> {
    context: &'a Context,
    eg: EGraph<Node>,
    value_class: HashMap<ValueId, Id>,
    arg_block: HashMap<ValueId, BlockId>,
    dom: Rc<DominatorTree>,
    ruleset: Ruleset,
}

impl Driver<'_> {
    fn process_region(
        &mut self,
        region: RegionId,
        rewriter: &mut Rewriter,
    ) -> Result<(), PassError> {
        self.eg
            .saturate(&self.ruleset.rewrites, ITER_LIMIT, NODE_LIMIT);
        let extraction = self.eg.extract_best(node::cost);

        let op_ids: Vec<OpId> = self
            .context
            .get_region(region)
            .iter(self.context.clone())
            .flat_map(|block| self.context.get_block(block.id()).op_ids())
            .collect();
        for &op_id in &op_ids {
            self.rewrite_op(op_id, &extraction, rewriter)?;
        }
        self.recurse(&op_ids, rewriter)
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
        // A constant materializes to itself; nothing else is a single-result candidate.
        if instance.results.len() != 1
            || instance
                .clone()
                .as_interface::<dyn ConstantLike>()
                .is_some()
        {
            return Ok(());
        }
        let value = instance.results[0];
        let Some(&class) = self.value_class.get(&value) else {
            return Ok(());
        };
        let ty = self.context.get_value(value).ty();
        let block = instance.parent_block().map(|b| self.context.get_block(b));
        let target = OperationRef::new(instance.clone(), block, None);
        let mut memo = HashMap::new();
        let new_value = self.materialize(extraction, class, ty, &target, rewriter, &mut memo)?;
        // The replacement must dominate the use it takes over. Operand reuse and
        // freshly built ops satisfy this by construction; a cross-block CSE or a gate
        // collapsing to an arm may not, so check before committing.
        if new_value != value && self.dominates(new_value, value) {
            self.context.replace_value_uses(value, new_value);
            // Only erase a pure value op; an op with regions may have side effects
            // whose result merely became unused (left for DCE).
            if instance.regions.is_empty() {
                rewriter.erase_op(&target)?;
            }
        }
        Ok(())
    }

    /// Whether the def of `a` dominates the def of `b`. `b` is always an op result,
    /// so it has a defining op; `a` may be a block argument, located via `arg_block`.
    fn dominates(&self, a: ValueId, b: ValueId) -> bool {
        let (Some(ab), Some(bb)) = (self.def_block(a), self.def_block(b)) else {
            return false;
        };
        if ab != bb {
            return self.dom.dominates(ab, bb);
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

    fn def_block(&self, value: ValueId) -> Option<BlockId> {
        match self.context.get_value(value).defining_op() {
            Some(op) => self.context.get_op(op).parent_block(),
            None => self.arg_block.get(&value).copied(),
        }
    }

    /// Recurse into each nested region, assuming a guard's fact inside its region.
    fn recurse(&mut self, op_ids: &[OpId], rewriter: &mut Rewriter) -> Result<(), PassError> {
        for &op_id in op_ids {
            if !self.context.has_operation(op_id) {
                continue;
            }
            let instance = self.context.get_op(op_id);
            if instance.regions.is_empty() {
                continue;
            }
            let guarded = instance
                .clone()
                .as_interface::<dyn RegionGuard>()
                .map(|g| g.guarded_regions())
                .unwrap_or_default();
            for &sub in &instance.regions {
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
        let constant = self.eg.add(Node::Const {
            value: APInt::new(1, holds as u64),
            origin: None,
        });
        self.eg.union(cond, constant);
        self.eg.rebuild();
    }

    /// Rebuild the value of `class`'s cheapest node: an existing value is reused, a
    /// constant or rule-introduced op is built before `target`. Memoized per class.
    fn materialize(
        &self,
        extraction: &Extraction<Node>,
        class: Id,
        expected_ty: TypeId,
        target: &OperationRef,
        rewriter: &mut Rewriter,
        memo: &mut HashMap<Id, ValueId>,
    ) -> Result<ValueId, PassError> {
        let class = self.eg.find(class);
        if let Some(&value) = memo.get(&class) {
            return Ok(value);
        }
        let node = extraction.node(class).expect("extracted class has a node");
        let value = match node {
            // A gate is never rebuilt; it stands for its block-argument value.
            Node::Gate(gate, _) => gate_value(gate),
            Node::Const { value, origin } => match origin {
                Some(op) => self.context.get_op(*op).results[0],
                None => {
                    let op = ops::constant(self.context, value.to_i64(), expected_ty).build();
                    rewriter.insert_op_before(target, &op)?;
                    op.result()
                }
            },
            Node::Op { prov, args, ty, .. } => match prov {
                OpProv::Seeded(op) => self.context.get_op(*op).results[0],
                OpProv::Introduced(idx) => {
                    let mut operands = Vec::with_capacity(args.len());
                    for &arg in args {
                        operands
                            .push(self.materialize(extraction, arg, *ty, target, rewriter, memo)?);
                    }
                    let emit = self.ruleset.emits[*idx]
                        .as_ref()
                        .expect("an introduced op supplies an emit");
                    emit(self.context, &operands, *ty, target, rewriter)?
                }
            },
        };
        memo.insert(class, value);
        Ok(value)
    }
}
