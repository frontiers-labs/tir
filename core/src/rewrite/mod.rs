//! The rewrite driver instcombine and instruction selection share: a
//! function's values seeded into one e-graph, saturated with a pass's rules,
//! extracted by the pass's cost, and committed back without an order to place
//! into.
//!
//! A rewrite hands a value's readers the cheapest spelling of its class. What
//! that spelling is in scope of is the region tree and the dependences between
//! siblings: a value defined in the reader's region, or in one enclosing it and
//! not downstream of the operation holding the reader, is read as it is; one
//! defined in an arm the reader cannot see is rebuilt where the reader sits, if
//! the operation computing it cannot trap, and left where it is otherwise. A
//! rule-introduced op or a literal is built and placed where its operands say.
//! Nothing here asks where an operation sits in its region or which came first.
//!
//! A region keeps the order its operations came in, which is what later breaks
//! the ties between operations that do not depend on each other. What the
//! commit writes for an IR value joins that order at the operation defining
//! the value, whichever reader asked for it.
//!
//! A read whose value was rewritten and a write overwritten unread leave
//! their states to the readers, so the sweep takes them.
//!
//! What a pass adds is a [`Rewrite`]: its rules, its cost and how a node it
//! chose is written.

use std::cell::RefCell;
use std::sync::Arc;

use smallvec::SmallVec;
use tir_egraph::{
    Atom, ClassId as Id, ColumnId, Engine, Externs, Extraction, Label, Ref, RowId, Rule,
};
use tir_relational::Words;

use crate::analysis::effects::{observed_state, produced_state};
use crate::binding::{forwards_state, state_chains};
use crate::operation::OpNameId;
use crate::sem::{Kind, Prov, SemNode as Node, SymKind};
use crate::utils::APInt;
use crate::{
    ConstantLike, Context, Gamma, IdMap, MemoryRead, MemoryWrite, NewOp, OpHandle, OpId, PassError,
    RegionId, Speculatable, TypeId, ValueId,
    attributes::{AttributeValue, Predicate},
    builtin::ops,
};

mod seed;

pub(crate) use seed::{Kinds, LoopPorts, Port, Seeded, children, seed, seed_ops};

/// `Load(address, bytes, metadata, state)`.
pub(crate) const LOAD_ARITY: usize = 4;
/// `Store(address, bytes, value, address_space, state)`.
pub(crate) const STORE_ARITY: usize = 5;
pub(crate) const ADDRESS: usize = 0;
pub(crate) const BYTES: usize = 1;

const ITER_LIMIT: usize = 30;
// How far saturation may grow the graph it was given: a seeded graph past a
// fixed size would otherwise be left unsearched.
const NODE_GROWTH: usize = 100_000;

/// The spelling a reference already has, so a form rebuilt for one reader
/// answers every reader that can see it. `add_auto` places a rebuilt form where
/// its operands allow, which may be well outside the reader's own region.
pub(crate) type Memo = HashMap<Ref, ValueId>;

type HashMap<K, V> = std::collections::HashMap<K, V, tir_adt::FxBuildHasher>;

// In the order written.
type Written = HashMap<RegionId, Vec<(OpId, OpId)>>;

pub(crate) struct RuleSet {
    // In the order their heads apply.
    pub(crate) rules: Vec<Arc<Rule<Node>>>,
    pub(crate) tables: Vec<Arc<Words>>,
    pub(crate) externs: Box<dyn Externs<Node>>,
}

pub(crate) trait Rewrite {
    fn rules(&self) -> &RuleSet;

    // Infinite for a node the pass cannot write back.
    fn cost(&self, eg: &Engine<Node>, class: Id, node: &Node, operands: &[Ref], offset: u64)
    -> u64;

    fn spell(&self, out: &mut Commit<'_>, row: RowId, region: RegionId) -> Option<ValueId> {
        out.builtin(row, region)
    }

    fn arm_facts(&self) -> bool;

    fn rewire_op(&self, _out: &mut Commit<'_>, _instance: &OpHandle, _region: RegionId) -> bool {
        false
    }

    // Whether the rules lower what they match: no node a rule's root matches
    // is written back; see `Driver::lower`.
    fn lowers(&self) -> bool {
        false
    }
}

/// Rewrites each region under the assumptions that hold there, and *before* its
/// children's scopes open so the base classes a child scope reads are final.
pub(crate) struct Driver<'a, R: Rewrite> {
    pub(crate) context: &'a Context,
    pub(crate) eg: Engine<Node>,
    pub(crate) value_class: IdMap<ValueId, Ref>,
    pass: &'a R,
    name: &'static str,
    /// The IR values each class of the final base graph is, at their offsets
    /// from it, in value order.
    named: HashMap<Id, Vec<(u64, ValueId)>>,
    /// For each operation of a region whose nested regions are being
    /// committed, the most operations with regions any dependence path ending
    /// at it passes through, itself included. It never falls along a
    /// dependence, so what depends on an operation counts at least as many.
    carriers: IdMap<OpId, usize>,
    written: RefCell<Written>,
    // The roots of a lowering's rules; see `Rewrite::lowers`.
    lowered: Vec<Node>,
}

pub(crate) fn rule_roots(rules: &[Arc<Rule<Node>>]) -> Vec<Node> {
    let root = |rule: &Arc<Rule<Node>>| {
        let query = rule.plan.query();
        query.atoms.iter().find_map(|atom| match atom {
            Atom::Node {
                template, class, ..
            } if *class == query.root => Some(template.clone()),
            _ => None,
        })
    };
    rules.iter().filter_map(root).collect()
}

pub(crate) fn root_ops(context: &Context, roots: &[Node]) -> Vec<OpNameId> {
    let op = |root: &Node| match &root.kind {
        Kind::Ir(op) => Some(context.intern_op_name(op.dialect, op.name)),
        _ => None,
    };
    roots.iter().filter_map(op).collect()
}

impl<'a, R: Rewrite> Driver<'a, R> {
    pub(crate) fn new(
        context: &'a Context,
        seeded: Seeded,
        pass: &'a R,
        name: &'static str,
    ) -> Self {
        let mut eg = seeded.eg;
        for table in &pass.rules().tables {
            eg.add_words(table.clone());
        }
        let lowered = match pass.lowers() {
            true => rule_roots(&pass.rules().rules),
            false => Vec::new(),
        };

        Self {
            context,
            eg,
            value_class: seeded.value_class,
            pass,
            name,
            named: HashMap::default(),
            carriers: IdMap::new(),
            written: RefCell::default(),
            lowered,
        }
    }

    pub(crate) fn saturate(&mut self) {
        let set = self.pass.rules();
        crate::sem::workload::dump(self.name, &self.eg, &set.rules);
        let limit = self.eg.total_size().saturating_add(NODE_GROWTH);
        self.eg
            .saturate_rules(&set.rules, &*set.externs, ITER_LIMIT, limit);
        crate::memstats::egraph_census(self.name, &self.eg);
    }

    pub(crate) fn extract(&self) -> Extraction<'static, Node> {
        self.eg
            .extract_best(|class, node, operands, offset| self.price(class, node, operands, offset))
    }

    fn refresh<'b>(
        &self,
        extraction: &'b Extraction<'b, Node>,
        dirty: &[Id],
    ) -> Extraction<'b, Node> {
        extraction.refresh(&self.eg, dirty, |class, node, operands, offset| {
            self.price(class, node, operands, offset)
        })
    }

    fn price(&self, class: Id, node: &Node, operands: &[Ref], offset: u64) -> u64 {
        if self.lowered.iter().any(|root| root.matches_template(node)) {
            return u64::MAX;
        }
        self.pass.cost(&self.eg, class, node, operands, offset)
    }

    pub(crate) fn hypothesize(&mut self, loops: Vec<LoopPorts>) {
        let mut order: Vec<usize> = (0..loops.len()).collect();
        order.sort_by_key(|&index| self.nesting(loops[index].op));
        self.hypothesize_within(&loops, &order, None);
    }

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
                    let _ = self.eg.union(port.head, port.init);
                }
                self.eg.rebuild();
                let set = self.pass.rules();
                let limit = self.eg.total_size().saturating_add(NODE_GROWTH);
                self.eg
                    .saturate_rules(&set.rules, &*set.externs, ITER_LIMIT, limit);
                self.hypothesize_within(loops, order, Some(holder.op));
                let refuted: Vec<bool> = hypotheses
                    .iter()
                    .map(|port| {
                        port.edges
                            .iter()
                            .any(|&edge| !self.eg.connected(edge, port.init))
                    })
                    .collect();
                self.eg.pop_context();
                if !refuted.contains(&true) {
                    break;
                }
                let mut dropped = refuted.iter();
                hypotheses.retain(|_| !dropped.next().copied().unwrap_or(false));
            }
            let promoted = !hypotheses.is_empty();
            for port in &hypotheses {
                let _ = self.eg.union(port.head, port.init);
            }
            self.eg.rebuild();
            // Back to a fixpoint before the next loop opens its scope, from the
            // log the promoted unions left. A loop that promoted nothing left
            // none, and a saturation still costs a round of the rules no delta
            // narrows.
            if promoted {
                self.saturate();
                let mut published = false;
                for port in hypotheses {
                    // Substitution can make the exit constant. A value that
                    // still depends on another changing port stays in the loop.
                    if self.is_constant(port.published) {
                        let _ = self.eg.union(port.result, port.published);
                        published = true;
                    }
                }
                if published {
                    self.eg.rebuild();
                    self.saturate();
                }
            }
            self.hypothesize_within(loops, order, Some(holder.op));
        }
    }

    fn enclosing_loop(&self, loops: &[LoopPorts], op: OpId) -> Option<OpId> {
        let mut current = self.context.parent_op(op);
        while let Some(holder) = current {
            if loops.iter().any(|other| other.op == holder) {
                return Some(holder);
            }
            current = self.context.parent_op(holder);
        }
        None
    }

    fn is_constant(&self, value: Ref) -> bool {
        self.eg.int_const(value).is_some()
    }

    fn nesting(&self, op: OpId) -> usize {
        let mut depth = 0;
        let mut current = self.context.parent_op(op);
        while let Some(holder) = current {
            depth += 1;
            current = self.context.parent_op(holder);
        }
        depth
    }

    fn inject(&mut self, value: ValueId, width: u32, bits: u64) {
        let class = self.class_of(value);
        let constant = self.eg.add(Node::constant(
            APInt::new(u64::from(width), bits),
            Prov::None,
        ));
        let _ = self.eg.union(class, constant);
        if width == 1
            && let Some((lhs, rhs)) = self.settled_equality(value, bits == 1)
        {
            let _ = self.eg.union(lhs, rhs);
        }
        self.eg.rebuild();
    }

    fn settled_equality(&mut self, value: ValueId, holds: bool) -> Option<(Ref, Ref)> {
        let op = self.context.get_value(value).defining_op()?;
        let instance = self.context.get_op(op);
        if !instance.is::<ops::CmpIOp>() {
            return None;
        }
        let AttributeValue::Predicate(predicate) = instance.attr("predicate")? else {
            return None;
        };
        let equal = match predicate {
            Predicate::Eq => holds,
            Predicate::Ne => !holds,
            _ => return None,
        };
        let [lhs, rhs] = instance.operands()[..] else {
            return None;
        };
        equal.then(|| (self.class_of(lhs), self.class_of(rhs)))
    }

    fn class_of(&mut self, value: ValueId) -> Ref {
        if let Some(&known) = self.value_class.get(&value) {
            return known;
        }
        let leaf = self.eg.add(Node::input(value));
        self.value_class.insert(value, leaf);
        leaf
    }

    pub(crate) fn commit(
        &mut self,
        region: RegionId,
        extraction: &Extraction<'_, Node>,
        memo: &mut Memo,
    ) -> Result<(), PassError> {
        let pass = self.pass;
        let handle = self.context.get_region(region);
        let mut out = self.out(extraction, memo);
        for port in handle.value_arguments() {
            out.rewire(port.id(), region)?;
        }
        let ops = handle.op_ids();
        for &op in &ops {
            let instance = self.context.get_op(op);
            if pass.rewire_op(&mut out, &instance, region) {
                continue;
            }
            if instance.is::<crate::state::JoinOp>()
                && let Some(merged) = merged_state(self.context, &instance)
            {
                for result in instance.state_results() {
                    self.context.replace_value_uses(result, merged);
                    self.context
                        .rename_region_results(region, result, merged, &[]);
                }
                continue;
            }
            if instance.has_interface::<dyn ConstantLike>() {
                continue;
            }
            for result in instance.value_results() {
                if out.rewire(result, region)?.is_some() {
                    forward_read_state(self.context, &instance, region);
                }
            }
        }
        // Once every read the rewrites answered has let go of its state, a
        // write nothing else observes is unread.
        for &op in &ops {
            forward_dead_write(&out, op, region);
        }
        self.count_carriers(region)?;
        for &op in &ops {
            let instance = self.context.get_op(op);
            let Some(gamma) = instance.clone().as_interface::<dyn Gamma>() else {
                for sub in instance.regions() {
                    self.commit(sub, extraction, memo)?;
                }
                continue;
            };
            // An arm runs where the predicate equals its case; a boolean's
            // second arm, its default, runs where the predicate is 1.
            let predicate = gamma.predicate();
            let width = self
                .context
                .terms()
                .bit_width(self.context.get_value(predicate).ty())
                .map(|width| width as u32);
            let arms = gamma.arms();
            let boolean = width == Some(1) && arms.len() == 2;
            let cases = gamma.cases();
            for (index, arm) in arms.into_iter().enumerate() {
                let case = if boolean {
                    Some(index as u64)
                } else {
                    cases.get(index).copied()
                };
                let (Some(width), Some(case)) = (width, case) else {
                    self.commit(arm, extraction, memo)?;
                    continue;
                };
                if !pass.arm_facts() {
                    self.commit(arm, extraction, memo)?;
                    continue;
                }
                self.eg.push_context();
                self.inject(predicate, width, case);
                self.saturate();
                let dirty = self.eg.innermost_dirty();
                let scoped = self.refresh(extraction, &dirty);
                // A spelling built under this arm's fact answers only here.
                let mut scoped_memo = memo.clone();
                self.commit(arm, &scoped, &mut scoped_memo)?;
                self.eg.pop_context();
            }
        }
        self.place(region);
        Ok(())
    }

    pub(crate) fn lower(
        &mut self,
        ops: &[OpId],
        extraction: &Extraction<'_, Node>,
    ) -> Result<(), PassError> {
        let context = self.context;
        let mut memo = Memo::default();
        let mut regions = Vec::new();
        {
            let mut out = self.out(extraction, &mut memo);
            for &op in ops {
                let region = context.region_of_op(op).expect("an op of the function");
                out.lower(&context.get_op(op), region)?;
                if !regions.contains(&region) {
                    regions.push(region);
                }
            }
        }
        for &region in &regions {
            self.place(region);
        }
        for &op in ops {
            let instance = context.get_op(op);
            let region = context.region_of_op(op).expect("an op of the function");
            let unread =
                |&value: &ValueId| !context.is_used(value) && !published(context, region, value);
            if instance.results().iter().all(unread) {
                context.erase_op(&crate::OperationRef::new(instance))?;
            }
        }
        Ok(())
    }

    // The region keeps the order its operations came in.
    fn place(&self, region: RegionId) {
        let Some(written) = self.written.borrow_mut().remove(&region) else {
            return;
        };
        let mut ops = self.context.get_region(region).op_ids();
        let mut places: HashMap<OpId, usize> = ops.iter().copied().zip(0..).collect();
        for (op, covered) in written {
            places.insert(op, places[&covered]);
        }
        ops.sort_by_key(|op| places[op]);
        self.context.set_region_ops(region, ops);
    }

    fn count_carriers(&mut self, region: RegionId) -> Result<(), PassError> {
        let context = self.context;
        let ops = context.get_region(region).op_ids();
        let holds: Vec<usize> = context.with_rows(|rows| {
            let holds = ops.iter().map(|&op| rows.op_cells(op).owns_regions());
            holds.map(usize::from).collect()
        });
        let holders = holds.iter().sum::<usize>();
        if holders == 0 {
            return Ok(());
        }
        let mut counts: HashMap<OpId, usize> = ops.iter().copied().zip(holds).collect();
        let mut reads: Vec<(OpId, usize, OpId)> = Vec::new();
        for &producer in &ops {
            for value in context.get_op(producer).results() {
                let operands = context.uses_of(value).into_iter().map(|read| read.op);
                let published = context
                    .result_uses(value)
                    .into_iter()
                    .filter(|&(naming, _)| naming != region)
                    .filter_map(|(naming, _)| context.get_region(naming).parent_op());
                for reader in operands.chain(published) {
                    if let Some(reader) = self.held_in(reader, region) {
                        reads.push((reader, counts[&reader], producer));
                    }
                }
            }
        }
        let mut rose = true;
        while rose {
            rose = false;
            for &(reader, own, producer) in &reads {
                let count = counts[&producer] + own;
                if count <= counts[&reader] {
                    continue;
                }
                if count > holders {
                    let instance = context.get_op(reader);
                    return Err(PassError::InvalidIR {
                        pass: self.name,
                        error: crate::Error::VerificationError(format!(
                            "{}.{} closes a dependency cycle",
                            instance.dialect(),
                            instance.name()
                        )),
                    });
                }
                counts.insert(reader, count);
                rose = true;
            }
        }
        self.carriers.extend(counts);
        Ok(())
    }

    fn held_in(&self, op: OpId, region: RegionId) -> Option<OpId> {
        let mut current = op;
        while self.context.region_of_op(current)? != region {
            current = self.context.parent_op(current)?;
        }
        Some(current)
    }

    fn out<'c>(&'c self, extraction: &'c Extraction<'c, Node>, memo: &'c mut Memo) -> Commit<'c> {
        Commit {
            context: self.context,
            eg: &self.eg,
            value_class: &self.value_class,
            extraction,
            memo,
            pass: self.pass,
            named: &self.named,
            carriers: &self.carriers,
            written: &self.written,
            spelled: SmallVec::new(),
            replacing: None,
            wanted: None,
        }
    }

    pub(crate) fn index_values(&mut self) {
        let mut named: HashMap<Id, Vec<(u64, ValueId)>> = HashMap::default();
        let mut values: Vec<(&ValueId, &Ref)> = self.value_class.iter().collect();
        values.sort_by_key(|(value, _)| value.number());
        for (&value, &reference) in values {
            let found = self.eg.find(reference);
            named
                .entry(found.class)
                .or_default()
                .push((found.offset, value));
        }
        self.named = named;
    }
}

pub(crate) struct Commit<'c> {
    pub(crate) context: &'c Context,
    pub(crate) eg: &'c Engine<Node>,
    pub(crate) value_class: &'c IdMap<ValueId, Ref>,
    pub(crate) extraction: &'c Extraction<'c, Node>,
    pub(crate) memo: &'c mut Memo,
    pass: &'c dyn Rewrite,
    named: &'c HashMap<Id, Vec<(u64, ValueId)>>,
    carriers: &'c IdMap<OpId, usize>,
    written: &'c RefCell<Written>,
    // Innermost last.
    spelled: SmallVec<[OpId; 4]>,
    /// The value whose readers are being rewired. It answers for its own class
    /// and would answer every rewrite with itself, so no spelling may pick it.
    replacing: Option<ValueId>,
    wanted: Option<(Ref, TypeId)>,
}

impl Commit<'_> {
    pub(crate) fn wanted(&self) -> (Ref, TypeId) {
        self.wanted.expect("a pass spells what the commit asks for")
    }

    pub(crate) fn sits(&self, row: RowId) -> bool {
        let (reference, _) = self.wanted();
        self.eg.int_const(reference).is_none() && self.eg.value(row) == reference
    }

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

    pub(crate) fn rewire(
        &mut self,
        value: ValueId,
        region: RegionId,
    ) -> Result<Option<ValueId>, PassError> {
        let read = || self.context.is_used(value) || published(self.context, region, value);
        if self.value_class.contains_key(&value) && !read() {
            return Ok(None);
        }
        self.rewire_read(value, region)
    }

    fn lower(&mut self, instance: &OpHandle, region: RegionId) -> Result<(), PassError> {
        let class = |out: &Self, value: &ValueId| Some(out.eg.find(*out.value_class.get(value)?));
        let read: Vec<_> = (instance.operands().iter())
            .filter_map(|value| Some((class(self, value)?, *value)))
            .collect();
        let outer: Vec<_> = (read.into_iter())
            .map(|(class, value)| (class, self.memo.insert(class, value)))
            .collect();
        for result in instance.results() {
            if let Some(class) = class(self, &result) {
                self.memo.remove(&class);
            }
            self.rewire(result, region)?;
        }
        for (class, value) in outer.into_iter().rev() {
            match value {
                Some(value) => self.memo.insert(class, value),
                None => self.memo.remove(&class),
            };
        }
        Ok(())
    }

    pub(crate) fn rewire_read(
        &mut self,
        value: ValueId,
        region: RegionId,
    ) -> Result<Option<ValueId>, PassError> {
        let trace = |why: &str| {
            if std::env::var_os("TIR_REWRITE_TRACE").is_some() {
                eprintln!("rewrite-kept: {value:?} {why}");
            }
        };
        let Some(&reference) = self.value_class.get(&value) else {
            trace("is not seeded");
            return Ok(None);
        };
        let ty = self.context.get_value(value).ty();
        self.replacing = Some(value);
        let spelled = self.materialize_as(value, reference, ty, region);
        self.replacing = None;
        let Some(new_value) = spelled else {
            trace("has no spelling");
            return Ok(None);
        };
        if new_value == value {
            trace("spells itself");
            return Ok(None);
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
        Ok(Some(new_value))
    }

    pub(crate) fn materialize_as(
        &mut self,
        named: ValueId,
        reference: Ref,
        ty: TypeId,
        region: RegionId,
    ) -> Option<ValueId> {
        let defining = self.context.get_value(named).defining_op();
        self.spelled.extend(defining);
        // What spells `named` is located where its defining op is.
        let origin = defining.and_then(|op| self.context.op_location(op));
        let outer = origin.map(|origin| self.context.set_origin(Some(origin)));
        let value = self.materialize(reference, ty, region);
        if let Some(outer) = outer {
            self.context.set_origin(outer);
        }
        self.spelled
            .truncate(self.spelled.len() - defining.iter().len());
        value
    }

    pub(crate) fn add(&self, region: RegionId, op: OpId) {
        self.context.add(region, op);
        self.wrote(region, op);
    }

    fn add_auto(&self, op: OpId) {
        let region = self.context.add_auto(op);
        self.wrote(region, op);
    }

    fn wrote(&self, region: RegionId, op: OpId) {
        let held = |covered: &&OpId| self.context.region_of_op(**covered) == Some(region);
        if let Some(&covered) = self.spelled.iter().rev().find(held) {
            let mut written = self.written.borrow_mut();
            written.entry(region).or_default().push((op, covered));
        }
    }

    pub(crate) fn materialize(
        &mut self,
        reference: Ref,
        ty: TypeId,
        region: RegionId,
    ) -> Option<ValueId> {
        let reference = self.eg.find(reference);
        if let Some(&value) = self.memo.get(&reference)
            && self.readable(value, region)
        {
            return Some(value);
        }
        let Some(row) = self.extraction.row(reference.class) else {
            if std::env::var_os("TIR_REWRITE_TRACE").is_some() {
                eprintln!("rewrite-unspelled: no row of {reference:?} is extracted");
            }
            return None;
        };
        let outer = self.wanted.replace((reference, ty));
        let pass = self.pass;
        let value = pass.spell(self, row, region);
        self.wanted = outer;
        self.memo.insert(reference, value?);
        value
    }

    // `None` where the reference has no spelling in `region`: the value being
    // rewritten itself, or an op
    // that may trap sitting in a region the reader cannot see.
    pub(crate) fn builtin(&mut self, row: RowId, region: RegionId) -> Option<ValueId> {
        let (reference, ty) = self.wanted();
        if let Some(bits) = self.eg.int_const(reference) {
            if let Some(value) = self.named_at(reference, 0, ty, region) {
                return Some(value);
            }
            if !self.is_pointer(ty) {
                let width = self.eg.width(reference.class)?;
                let value = APInt::new_signed(u64::from(width), bits as i64);
                return Some(self.literal(&value, ty, region));
            }
            if bits == 0 {
                return self.null(reference, ty, region);
            }
        }
        let at = self.eg.value(row).offset;
        if at == reference.offset {
            return self.node(row, region);
        }
        if let Some(value) = self.named_at(reference, at, ty, region) {
            return Some(value);
        }
        let base = self.materialize(Ref::new(reference.class, at), ty, region)?;
        self.add_offset(base, reference, at, ty, region)
    }

    fn literal(&self, value: &APInt, ty: TypeId, region: RegionId) -> ValueId {
        let op = crate::builtin::ops::literal(self.context, value, ty).build();
        self.add(region, crate::Operation::id(&op));
        op.result()
    }

    // The driver has no builder for a null pointer, so it is a copy of a
    // literal the function has.
    fn null(&self, reference: Ref, ty: TypeId, region: RegionId) -> Option<ValueId> {
        let named = self.named.get(&reference.class)?.iter();
        let literal = named
            .filter(|&&(offset, _)| offset == reference.offset)
            .find_map(|&(_, value)| {
                let op = self.context.get_value(value).defining_op()?;
                let instance = self.context.get_op(op);
                instance
                    .has_interface::<dyn ConstantLike>()
                    .then_some(instance)
            })?;
        let result = self.context.create_value(ty, None).id();
        let copy = self.context.add_operation(NewOp::new_dynamic(
            (literal.dialect().as_str(), literal.name().as_str()),
            self.context.clone(),
            vec![],
            vec![result],
            vec![],
            literal.attributes().to_vec(),
        ));
        self.context.add(region, copy.id);
        Some(result)
    }

    fn is_pointer(&self, ty: TypeId) -> bool {
        crate::ptr::PtrType::view(&self.context, ty).is_some()
    }

    fn node(&mut self, row: RowId, region: RegionId) -> Option<ValueId> {
        let eg = self.eg;
        let node = eg.node(row);
        match node.prov {
            Prov::Value(_) | Prov::Op(_) => {
                let named = self.named_value(node)?;
                // An op whose operands carried offsets its laws moved out is
                // the row at the sum of them, not the row: the row is the op
                // over the operands' classes, rebuilt.
                if !self
                    .value_class
                    .get(&named)
                    .is_some_and(|&value| eg.find(value) == eg.value(row))
                {
                    return self.rebuild(row, region);
                }
                if Some(named) == self.replacing {
                    return None;
                }
                if self.readable(named, region) {
                    if std::env::var_os("TIR_REWRITE_TRACE").is_some() {
                        eprintln!("rewrite-reused: {named:?} spells {:?}", node.kind);
                    }
                    return Some(named);
                }
                self.rebuild(row, region)
            }
            Prov::Introduced => self.introduce(row, region),
            Prov::None => Some(self.literal(node.int()?, self.wanted().1, region)),
        }
    }

    // The op a rule introduced over its operands' spellings, each read at the
    // type its class holds, placed where they say. An operand whose class
    // holds no type leaves the op unwritten.
    fn introduce(&mut self, row: RowId, region: RegionId) -> Option<ValueId> {
        let node = self.eg.node(row);
        let (Kind::Ir(op), Some(ty)) = (&node.kind, node.ty) else {
            return None;
        };
        let types: Vec<TypeId> = self
            .eg
            .children(row)
            .iter()
            .map(|&child| self.class_type(child))
            .collect::<Option<_>>()?;
        let operands = self.operands(row, &types, region)?;
        let mut attributes = op.attrs.clone();
        // A variadic op names how many operands each of its ports takes: its
        // variadic port, the states, takes what the others leave.
        let schema = self.context.op_schema(op.dialect, op.name);
        let ports = schema.map_or(&[][..], |schema| schema.operands);
        if ports.iter().any(|port| port.variadic) {
            let fixed = ports.iter().filter(|port| !port.variadic).count();
            let rest = operands.len().checked_sub(fixed)?;
            let sizes = ports
                .iter()
                .map(|port| AttributeValue::UInt(if port.variadic { rest as u64 } else { 1 }));
            let sizes = AttributeValue::Array(sizes.collect());
            attributes.push(self.context.named_attribute("operand_segment_sizes", sizes));
        }
        let result = self.context.create_value(ty, None).id();
        let written = self.context.add_operation(NewOp::new_dynamic(
            (op.dialect, op.name),
            self.context.clone(),
            operands,
            vec![result],
            vec![],
            attributes,
        ));
        // An introduced op joins its region after everything the region
        // holds, not at the op it replaces: that position is the tie-break
        // selection orders by, and moving it there cost instructions.
        match self.eg.children(row).is_empty() {
            true => self.context.add(region, written.id),
            false => {
                self.context.add_auto(written.id);
            }
        }
        Some(result)
    }

    // The type `reference`'s class holds. An integer constant's class is the
    // zero of its carrier, whose type is the integer of the carrier's width.
    fn class_type(&self, reference: Ref) -> Option<TypeId> {
        if let Some(key) = self.eg.fact(ColumnId::Type, reference.class) {
            return Some(TypeId::from_number(key as u32));
        }
        let found = self.eg.find(reference);
        let width = self.eg.width(found.class)?;
        let integer = crate::sem::node::int_carrier(width);
        let constant = self.eg.int_const(found).is_some()
            && self
                .eg
                .nodes(found.class)
                .all(|node| tir_egraph::Label::carrier(node) == integer);
        constant.then(|| crate::builtin::IntegerType::new(self.context, u64::from(width)))
    }

    fn named_at(&self, reference: Ref, at: u64, ty: TypeId, region: RegionId) -> Option<ValueId> {
        let mut members: SmallVec<[Id; 4]> =
            SmallVec::from_slice(self.eg.scope_members(reference.class));
        if members.is_empty() {
            members.push(reference.class);
        }
        members.sort_unstable();
        members.iter().find_map(|member| {
            self.named.get(member)?.iter().find_map(|&(offset, value)| {
                (self.eg.find(Ref::new(*member, offset)) == reference
                    && self.context.get_value(value).ty() == ty
                    && self.readable(value, region)
                    && self.spells(value, Ref::new(reference.class, at)))
                .then_some(value)
            })
        })
    }

    fn spells(&self, value: ValueId, base: Ref) -> bool {
        let Some(op) = self.context.get_value(value).defining_op() else {
            return false;
        };
        let instance = self.context.get_op(op);
        let constant = self.eg.int_const(base).is_some();
        if constant && instance.has_interface::<dyn ConstantLike>() {
            return true;
        }
        let add = !constant && instance.is::<crate::builtin::ops::AddIOp>();
        if !add && !instance.is::<crate::ptr::PtrAddOp>() {
            return false;
        }
        let operands: Option<Vec<Ref>> = instance
            .operands()
            .iter()
            .map(|operand| Some(self.eg.find(*self.value_class.get(operand)?)))
            .collect();
        match operands.as_deref() {
            Some(&[a, b]) => {
                (a == base && self.eg.int_const(b).is_some())
                    || (add && b == base && self.eg.int_const(a).is_some())
            }
            _ => false,
        }
    }

    fn add_offset(
        &self,
        base: ValueId,
        reference: Ref,
        at: u64,
        ty: TypeId,
        region: RegionId,
    ) -> Option<ValueId> {
        let width = self.eg.width(reference.class)?;
        let delta = reference.offset.wrapping_sub(at);
        let step = spell(&APInt::new_signed(u64::from(width), delta as i64));
        let pointer = self.is_pointer(ty);
        let step_ty = if pointer {
            crate::builtin::IntegerType::new(self.context, u64::from(width))
        } else {
            ty
        };
        let literal = crate::builtin::ops::constant(self.context, step, step_ty).build();
        self.add(region, crate::Operation::id(&literal));
        let (op, value) = if pointer {
            let op = crate::ptr::ptradd(self.context, base, literal.result(), ty).build();
            let value = op.result();
            (crate::Operation::id(&op), value)
        } else {
            let op = crate::builtin::ops::addi(self.context, base, literal.result(), ty).build();
            let value = op.result();
            (crate::Operation::id(&op), value)
        };
        self.add_auto(op);
        Some(value)
    }

    fn rebuild(&mut self, row: RowId, region: RegionId) -> Option<ValueId> {
        let node = self.eg.node(row);
        let Prov::Op(op) = node.prov else {
            return None;
        };
        let source = self.context.get_op(op);
        if !source
            .clone()
            .as_interface::<dyn Speculatable>()
            .is_some_and(|op| op.is_speculatable())
            || !source.regions().is_empty()
            || !source.state_operands().is_empty()
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
        let operands = self.operands(row, &types, region)?;
        let result = self.context.create_value(ty, None).id();
        let copy = self.context.add_operation(NewOp::new_dynamic(
            (source.dialect().as_str(), source.name().as_str()),
            self.context.clone(),
            operands,
            vec![result],
            vec![],
            source.attributes().to_vec(),
        ));
        self.add_auto(copy.id);
        Some(result)
    }

    pub(crate) fn operands(
        &mut self,
        row: RowId,
        types: &[TypeId],
        region: RegionId,
    ) -> Option<Vec<ValueId>> {
        let children = self.eg.children(row);
        let children = children.iter().zip(types);
        children
            .map(|(&arg, &ty)| self.materialize(arg, ty, region))
            .collect()
    }

    pub(crate) fn readable(&self, value: ValueId, region: RegionId) -> bool {
        let Some(defined) = crate::region::defining_region(self.context, value) else {
            return true;
        };
        let mut current = region;
        let mut holder = None;
        while current != defined {
            let Some(op) = self.context.get_region(current).parent_op() else {
                return false;
            };
            let Some(enclosing) = self.context.region_of_op(op) else {
                return false;
            };
            holder = Some(op);
            current = enclosing;
        }
        // A region the commit never entered, the module's, holds nothing the
        // commit could make depend on the function.
        let bound = holder.and_then(|holder| self.carriers.get(&holder));
        match (bound, self.context.get_value(value).defining_op()) {
            (Some(&bound), Some(op)) => self.carried(op) < bound,
            _ => true,
        }
    }

    fn carried(&self, op: OpId) -> usize {
        if let Some(&count) = self.carriers.get(&op) {
            return count;
        }
        let region = self.context.region_of_op(op);
        self.context
            .get_op(op)
            .operands()
            .iter()
            .filter_map(|&operand| self.context.get_value(operand).defining_op())
            .filter(|&producer| self.context.region_of_op(producer) == region)
            .map(|producer| self.carried(producer))
            .max()
            .unwrap_or(0)
    }
}
fn merged_state(context: &Context, join: &OpHandle) -> Option<ValueId> {
    let operands = join.state_operands();
    let (first, rest) = operands.split_first()?;
    if rest.iter().all(|other| other == first) {
        return Some(*first);
    }
    if !operands
        .iter()
        .all(|&state| context.users_of(state).as_slice() == [join.id])
    {
        return None;
    }
    let mut changed = operands
        .iter()
        .copied()
        .filter(|&state| changed_chain(context, state));
    let kept = changed.next();
    changed.next().is_none().then(|| kept.unwrap_or(*first))
}

fn forward_dead_write(out: &Commit<'_>, op: OpId, scope: RegionId) {
    let instance = out.context.get_op(op);
    if !instance.has_interface::<dyn MemoryWrite>() {
        return;
    }
    let (Some(taken), Some(leaves)) = (observed_state(&instance), produced_state(&instance)) else {
        return;
    };
    let Some(written) = extent(out, leaves) else {
        return;
    };
    let mut state = leaves;
    loop {
        if published(out.context, scope, state) {
            return;
        }
        let readers: Vec<OpId> = out
            .context
            .users_of(state)
            .into_iter()
            .filter(|&reader| !is_dead_read(out.context, scope, reader))
            .collect();
        if let Some(next) = disjoint_read_continuation(out, state, leaves, &readers, scope) {
            state = next;
            continue;
        }
        let [reader] = readers[..] else {
            return;
        };
        let handle = out.context.get_op(reader);
        // The write's own chain is the one it named first, so it is the
        // first the split hands back; a merge names the memory the change
        // taking it observes.
        if handle.is::<crate::state::SplitOp>() && state == handle.state_operands()[0]
            || handle.is::<crate::state::JoinOp>()
        {
            state = handle.state_results()[0];
            continue;
        }
        if !handle.has_interface::<dyn MemoryWrite>() {
            return;
        }
        let (Some(observed), Some(left)) = (observed_state(&handle), produced_state(&handle))
        else {
            return;
        };
        if observed != state {
            return;
        }
        if extent(out, left) != Some(written) {
            return;
        }
        break;
    }
    // The loop's first guard found no region result naming the state the
    // write leaves, so no result list here names it either.
    debug_assert!(!published(out.context, scope, leaves));
    out.context.replace_value_uses(leaves, taken);
}

fn disjoint_read_continuation(
    out: &Commit<'_>,
    state: ValueId,
    written: ValueId,
    readers: &[OpId],
    scope: RegionId,
) -> Option<ValueId> {
    let (object, offset, bytes) = extent(out, written)?;
    let mut outputs = Vec::new();
    let mut join = None;
    for &reader in readers {
        let instance = out.context.get_op(reader);
        if instance.is::<crate::state::JoinOp>() {
            if join
                .replace(reader)
                .is_some_and(|previous| previous != reader)
            {
                return None;
            }
            continue;
        }
        if instance.has_interface::<dyn MemoryWrite>() || observed_state(&instance) != Some(state) {
            return None;
        }
        let read = instance.clone().as_interface::<dyn MemoryRead>()?;
        let (read_object, read_offset, read_bytes) = extent(out, read.read_value())?;
        if object != read_object
            || !disjoint(out, object, (offset, bytes), (read_offset, read_bytes))
        {
            return None;
        }
        let output = produced_state(&instance)?;
        if published(out.context, scope, output) {
            return None;
        }
        if readers.len() == 1 {
            return Some(output);
        }
        let users = out.context.users_of(output);
        let [user] = users[..] else { return None };
        if !out.context.get_op(user).is::<crate::state::JoinOp>()
            || join.replace(user).is_some_and(|previous| previous != user)
        {
            return None;
        }
        outputs.push(output);
    }
    if outputs.is_empty() {
        return None;
    }
    let join = out.context.get_op(join?);
    if !join
        .state_operands()
        .iter()
        .all(|input| *input == state || outputs.contains(input))
    {
        return None;
    }
    join.state_results().first().copied()
}

fn extent(out: &Commit<'_>, value: ValueId) -> Option<(Id, u64, u64)> {
    let class = out.eg.find(*out.value_class.get(&value)?).class;
    let row = out
        .eg
        .rows(class)
        .find(|&row| out.eg.node(row).prov == Prov::Value(value))?;
    let arity = match out.eg.node(row).sym()? {
        SymKind::LoadMemory => LOAD_ARITY,
        SymKind::StoreMemory => STORE_ARITY,
        _ => return None,
    };
    let children = out.eg.children(row);
    if children.len() != arity {
        return None;
    }
    let address = children[ADDRESS];
    if out.eg.conflicted(address.class) {
        return None;
    }
    let bytes = out.eg.int_const(children[BYTES])?;
    Some((address.class, address.offset, bytes))
}

fn disjoint(
    out: &Commit<'_>,
    base: Id,
    (a, a_bytes): (u64, u64),
    (b, b_bytes): (u64, u64),
) -> bool {
    let mask = out
        .eg
        .width(base)
        .map_or(u64::MAX, |width| u64::MAX >> (64 - width));
    a_bytes != 0
        && b_bytes != 0
        && b.wrapping_sub(a) & mask >= a_bytes
        && a.wrapping_sub(b) & mask >= b_bytes
}

fn forward_read_state(context: &Context, instance: &OpHandle, region: RegionId) {
    if !instance.has_interface::<dyn MemoryRead>() {
        return;
    }
    if let (Some(observed), Some(published)) = (observed_state(instance), produced_state(instance))
    {
        context.replace_value_uses(published, observed);
        context.rename_region_results(region, published, observed, &[]);
    }
}

fn changed_chain(context: &Context, state: ValueId) -> bool {
    let Some(def) = context.get_value(state).defining_op() else {
        return true;
    };
    let instance = context.get_op(def);
    if instance.is::<crate::state::EntryStateOp>() {
        return false;
    }
    // The chains a function opens are one entry state split, so a split of a
    // memory nothing changed names one nothing changed either.
    if instance.is::<crate::state::SplitOp>() {
        return changed_chain(context, instance.state_operands()[0]);
    }
    let Some(chain) = state_chains(context, &instance)
        .into_iter()
        .find(|chain| chain.left == state)
    else {
        return true;
    };
    if !forwards_state(&chain) {
        return true;
    }
    changed_chain(context, chain.entered)
}

pub(crate) fn is_dead_read(context: &Context, scope: RegionId, op: OpId) -> bool {
    let instance = context.get_op(op);
    instance.has_interface::<dyn MemoryRead>()
        && !instance.has_interface::<dyn MemoryWrite>()
        && instance.results().iter().all(|&result| {
            context.users_of(result).is_empty() && !published(context, scope, result)
        })
}

fn spell(literal: &APInt) -> i64 {
    match literal.width() {
        1 => literal.to_u64() as i64,
        _ => literal.to_i64(),
    }
}

pub(crate) fn published(context: &Context, scope: RegionId, value: ValueId) -> bool {
    context.published_under(scope, value)
}
