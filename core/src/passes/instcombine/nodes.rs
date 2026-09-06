//! The simplifier over unordered regions: the same e-graph, seeded with the
//! declared loops and gates, committed without an order to place into.
//!
//! A rewrite hands a value's readers the cheapest spelling of its class. What
//! that spelling is in scope of is the region tree alone: a value defined in
//! the reader's region or one enclosing it is read as it is; one defined in an
//! arm the reader cannot see is rebuilt where the reader sits, if the operation
//! computing it cannot trap, and left where it is otherwise. A rule-introduced
//! op or a literal is built and placed where its operands say. Nothing here
//! asks where an operation sits in its region or which came first.
//!
//! The commit erases nothing on the way. The sweep after it erases every
//! operation the region's results do not demand, loops excepted: an effect in
//! no cone would never run, but a loop nobody reads may still not terminate.

use std::collections::{HashMap, HashSet};

/// The spelling a class already has, so a form rebuilt for one reader answers
/// every reader that can see it. `add_auto` places a rebuilt form where its
/// operands allow, which may be well outside the reader's own region.
type Memo = HashMap<Id, ValueId>;

use tir_relational::{ClassId as Id, Extraction};

use super::{Driver, Node, Prov, cost};
use crate::analysis::AnalysisManager;
use crate::func::FuncOp;
use crate::sem::egraph::type_width;
use crate::{
    ConstantLike, Context, Gamma, MemoryRead, NewOp, OpHandle, OpId, OperationRef, Pass, PassError,
    PassTarget, RegionId, RegionKind, Rewriter, Speculatable, Theta, TypeId, ValueId,
};

#[derive(Default)]
pub struct InstCombineNodesPass;

impl InstCombineNodesPass {
    pub fn new() -> Self {
        Self
    }
}

crate::register_pass!(InstCombineNodesPass, "instcombine-nodes");

impl Pass for InstCombineNodesPass {
    fn name(&self) -> &'static str {
        "instcombine-nodes"
    }

    fn target(&self) -> PassTarget {
        PassTarget::operation_on::<FuncOp>(RegionKind::Nodes)
    }

    fn run(
        &mut self,
        op: &OperationRef,
        context: &Context,
        rewriter: &mut Rewriter,
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        let root = op.op().id;
        let mut seeded = super::seed::seed(context, root);
        let loop_ports = std::mem::take(&mut seeded.loop_ports);
        let ruleset = super::builtin_ruleset(context, &seeded);
        let mut driver = Driver {
            context,
            eg: seeded.eg,
            value_class: seeded.value_class,
            arg_block: seeded.arg_block,
            ruleset,
            replacing: std::cell::Cell::new(None),
        };
        driver.saturate();
        driver.hypothesize(loop_ports);
        let extraction = driver.eg.extract_best(|_, node| cost(node));
        let body = context.get_op(root).regions()[0];
        driver.commit_nodes(body, &extraction, &mut HashMap::new())?;
        let result = sweep(context, body, rewriter);
        tir_relational::report_saturation("instcombine-nodes");
        result
    }
}

impl Driver<'_> {
    /// Rewire every port and every value result defined in `region`, then the
    /// regions nested in it, each arm of a boolean gate under the fact its
    /// predicate states there.
    fn commit_nodes(
        &mut self,
        region: RegionId,
        extraction: &Extraction<'_, Node>,
        memo: &mut Memo,
    ) -> Result<(), PassError> {
        let handle = self.context.get_region(region);
        for port in handle.value_arguments() {
            self.rewire_nodes(port.id(), region, extraction, memo)?;
        }
        let ops = handle.op_ids();
        for &op in &ops {
            let instance = self.context.get_op(op);
            if instance.has_interface::<dyn ConstantLike>() {
                continue;
            }
            for result in instance.value_results() {
                let replaced = self.rewire_nodes(result, region, extraction, memo)?;
                if replaced {
                    self.forward_read_state(&instance, region);
                }
            }
        }
        for &op in &ops {
            let instance = self.context.get_op(op);
            let arms = instance
                .clone()
                .as_interface::<dyn Gamma>()
                .filter(|gamma| {
                    gamma.arms().len() == 2
                        && type_width(self.context, self.context.get_value(gamma.predicate()).ty())
                            == Some(1)
                })
                .map(|gamma| (gamma.predicate(), gamma.arms()));
            match arms {
                Some((predicate, arms)) => {
                    for (index, arm) in arms.into_iter().enumerate() {
                        self.eg.push_context();
                        self.inject(predicate, index == 1);
                        self.saturate();
                        let dirty = self.eg.innermost_dirty();
                        let scoped = extraction.refresh(&self.eg, &dirty, |_, node| cost(node));
                        // A spelling built under this arm's fact answers only here.
                        let mut scoped_memo = memo.clone();
                        self.commit_nodes(arm, &scoped, &mut scoped_memo)?;
                        self.eg.pop_context();
                    }
                }
                None => {
                    for sub in instance.regions() {
                        self.commit_nodes(sub, extraction, memo)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// A read whose value was rewritten leaves memory as it found it: the state
    /// it published is the state it observed, so its readers take that, and the
    /// read is demanded by nothing.
    fn forward_read_state(&self, instance: &OpHandle, region: RegionId) {
        let Some(read) = instance.clone().as_interface::<dyn MemoryRead>() else {
            return;
        };
        if let (Some(observed), Some(published)) = (read.state_operand(), read.state_result()) {
            self.context.replace_value_uses(published, observed);
            self.context
                .rename_region_results(region, published, observed, &[]);
        }
    }

    /// Hand the readers of `value`, defined in `region`, the cheapest spelling
    /// of its class where that spelling is one the region can see. Answers
    /// whether anything changed.
    fn rewire_nodes(
        &mut self,
        value: ValueId,
        region: RegionId,
        extraction: &Extraction<'_, Node>,
        memo: &mut Memo,
    ) -> Result<bool, PassError> {
        let Some(&class) = self.value_class.get(&value) else {
            return Ok(false);
        };
        if !self.context.is_used(value) && !named_by_results(self.context, region, value) {
            return Ok(false);
        }
        let ty = self.context.get_value(value).ty();
        self.replacing.set(Some(value));
        let spelled = self.materialize_nodes(extraction, class, ty, region, memo);
        self.replacing.set(None);
        let Some(new_value) = spelled else {
            return Ok(false);
        };
        if new_value == value {
            return Ok(false);
        }
        self.context.replace_value_uses(value, new_value);
        // A port's own region may pin a result entry to the port — a counted
        // loop's exit is its port — so the entries a port's region names stay;
        // the regions nested in it read the port like any other value.
        if self.context.region_of_port(value) == Some(region) {
            for op in self.context.get_region(region).op_ids() {
                for sub in self.context.get_op(op).regions() {
                    self.context
                        .rename_region_results(sub, value, new_value, &[]);
                }
            }
        } else {
            self.context
                .rename_region_results(region, value, new_value, &[]);
        }
        Ok(true)
    }

    /// The value of `class`'s cheapest node where `region` can read it, or
    /// `None` where that node has no spelling there: the value being rewritten
    /// itself, or an op that may trap sitting in a region the reader cannot see.
    fn materialize_nodes(
        &self,
        extraction: &Extraction<'_, Node>,
        class: Id,
        expected_ty: TypeId,
        region: RegionId,
        memo: &mut Memo,
    ) -> Option<ValueId> {
        let class = self.eg.find(class);
        if let Some(&value) = memo.get(&class)
            && visible(self.context, value, region)
        {
            return Some(value);
        }
        let node = extraction.node(class)?;
        let value = match node.prov {
            Prov::Value(_) | Prov::Op(_) => {
                let named = self.named_value(node)?;
                if Some(named) == self.replacing.get() {
                    return None;
                }
                if visible(self.context, named, region) {
                    named
                } else {
                    self.rebuild(extraction, node, region, memo)?
                }
            }
            Prov::Introduced(idx) => {
                let ty = node.ty.expect("an op node carries its result type");
                let types = vec![ty; node.children.len()];
                let operands = self.materialize_children(extraction, node, &types, region, memo)?;
                let emit = self.ruleset.emits[idx]
                    .as_ref()
                    .expect("an introduced op supplies an emit");
                let (op, value) = emit(self.context, &operands, ty);
                self.context.add_auto(op.id());
                value
            }
            Prov::None => {
                let literal = node.int()?;
                let op =
                    crate::builtin::ops::constant(self.context, super::spell(literal), expected_ty)
                        .build();
                self.context.add(region, crate::Operation::id(&op));
                op.result()
            }
        };
        memo.insert(class, value);
        Some(value)
    }

    /// Spell a seeded op's value where `region` can read it: a copy of the op
    /// over its operands' spellings, placed where they say. Only an operation
    /// that cannot trap may run where the original would not have, and one
    /// without operands names no region to join.
    fn rebuild(
        &self,
        extraction: &Extraction<'_, Node>,
        node: &Node,
        region: RegionId,
        memo: &mut Memo,
    ) -> Option<ValueId> {
        let Prov::Op(op) = node.prov else {
            return None;
        };
        let source = self.context.get_op(op);
        if !source.has_interface::<dyn Speculatable>()
            || !source.regions().is_empty()
            || !source.dep_operands().is_empty()
            || source.operands().is_empty()
            || source.results().len() != 1
        {
            return None;
        }
        let ty = node.ty?;
        // Each operand is spelled at the type the original read it at: a
        // comparison's operands are not its boolean.
        let types: Vec<TypeId> = source
            .operands()
            .iter()
            .map(|&operand| self.context.get_value(operand).ty())
            .collect();
        if types.len() != node.children.len() {
            return None;
        }
        let operands = self.materialize_children(extraction, node, &types, region, memo)?;
        let result = self.context.create_value(ty, None).id();
        let copy = self.context.add_operation(NewOp::new_dynamic(
            (source.dialect().as_str(), source.name().as_str()),
            self.context.as_context_ref(),
            operands,
            vec![result],
            vec![],
            source.attributes().to_vec(),
        ));
        self.context.add_auto(copy.id);
        Some(result)
    }

    fn materialize_children(
        &self,
        extraction: &Extraction<'_, Node>,
        node: &Node,
        types: &[TypeId],
        region: RegionId,
        memo: &mut Memo,
    ) -> Option<Vec<ValueId>> {
        node.children
            .iter()
            .zip(types)
            .map(|(&arg, &ty)| self.materialize_nodes(extraction, arg, ty, region, memo))
            .collect()
    }
}

/// Whether `value` is in scope throughout `region`: defined in it, in a region
/// enclosing it, or at module level.
fn visible(context: &Context, value: ValueId, region: RegionId) -> bool {
    let Some(defined) = crate::region::defining_region(context, value) else {
        return true;
    };
    let mut current = Some(region);
    while let Some(here) = current {
        if here == defined {
            return true;
        }
        current = context
            .get_region(here)
            .parent_op()
            .and_then(|op| context.region_of_op(op));
    }
    false
}

/// Whether the result list of `region` or of any region nested in it names
/// `value`, which no use list records.
fn named_by_results(context: &Context, region: RegionId, value: ValueId) -> bool {
    context
        .nested_regions(region)
        .iter()
        .any(|&nested| context.get_region(nested).results().contains(&value))
}

/// Erase every operation under `body` its region's results do not demand.
/// Demand runs from the callable's results through operands and nested
/// regions' results; every loop is demanded outright, since erasing one erases
/// whether it terminates.
fn sweep(context: &Context, body: RegionId, rewriter: &mut Rewriter) -> Result<(), PassError> {
    let defining = |values: Vec<ValueId>| {
        values
            .into_iter()
            .filter_map(|value| context.get_value(value).defining_op())
            .collect::<Vec<_>>()
    };
    let mut worklist = defining(context.get_region(body).results());
    for region in context.nested_regions(body) {
        for op in context.get_region(region).op_ids() {
            if context.get_op(op).has_interface::<dyn Theta>() {
                let mut current = Some(op);
                while let Some(holder) =
                    current.filter(|&holder| holder != body_owner(context, body))
                {
                    worklist.push(holder);
                    current = context.parent_op(holder);
                }
            }
        }
    }
    let mut demanded: HashSet<OpId> = HashSet::new();
    while let Some(op) = worklist.pop() {
        if !demanded.insert(op) {
            continue;
        }
        let instance = context.get_op(op);
        worklist.extend(defining(instance.operands().to_vec()));
        for region in instance.regions() {
            worklist.extend(defining(context.get_region(region).results()));
        }
    }
    sweep_region(context, body, &demanded, rewriter)
}

fn body_owner(context: &Context, body: RegionId) -> OpId {
    context
        .get_region(body)
        .parent_op()
        .expect("a callable owns its body")
}

fn sweep_region(
    context: &Context,
    region: RegionId,
    demanded: &HashSet<OpId>,
    rewriter: &mut Rewriter,
) -> Result<(), PassError> {
    for op in context.get_region(region).op_ids() {
        if !demanded.contains(&op) {
            rewriter.erase_op(&OperationRef::new(context.get_op(op)))?;
            continue;
        }
        for sub in context.get_op(op).regions() {
            sweep_region(context, sub, demanded, rewriter)?;
        }
    }
    Ok(())
}
