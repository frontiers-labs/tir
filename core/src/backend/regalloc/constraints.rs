//! Register constraints (paper § 4). A constraint is the set of registers a
//! value may take: its class, narrowed by the classes naming it and by its
//! pin. The constraints of one file form a hierarchy (Definition 1). At a
//! constrained instruction the dying operands take the constraints of the
//! results (Algorithm 2), the operand constraints are relaxed again (Figure
//! 4), and the values live there are colored first, in the order of § 4.3.

use std::collections::{BTreeSet, HashMap, HashSet};

use tir::{Context, OpId, PassError, ValueId};

use super::db::{self, RegAllocDb};
use super::{RegClassId, RegisterInfo, allocation_copy_endpoints, allocation_order, resolve_class};
use crate::analysis::defuse::CLOBBERS_ATTR;
use crate::backend::abi::AbiInfo;
use crate::backend::liveness::{Classes, PhysReg, op_regs};
use crate::backend::registers::value_class;
use crate::backend::{ParallelCopyOp, RegSlot, reg_slots};

// The file indices a value may take, one bit each.
pub(super) type Set = u64;

// The root of every constraint hierarchy of a target: per file and group
// width, the registers the allocator hands out.
pub(super) struct Hierarchy {
    roots: Vec<(&'static str, u16, Set)>,
}

impl Hierarchy {
    // The hierarchies of `info`'s files under `abi`. Panics unless the
    // classes of every file nest (Definition 1).
    pub(super) fn new(info: &RegisterInfo, abi: &AbiInfo) -> Self {
        let sets: Vec<(RegClassId, BTreeSet<u16>)> = info
            .classes
            .iter()
            .map(RegClassId::new)
            .map(|class| {
                let set = allocation_order(abi, class)
                    .iter()
                    .map(|&(_, index)| index)
                    .collect();
                (class, set)
            })
            .filter(|(_, set): &(_, BTreeSet<u16>)| !set.is_empty())
            .collect();
        for (at, (a, sa)) in sets.iter().enumerate() {
            for (b, sb) in &sets[at + 1..] {
                let nested = sa.is_disjoint(sb) || sa.is_subset(sb) || sb.is_subset(sa);
                assert!(
                    a.file() != b.file() || nested,
                    "register classes {} and {} of file {} overlap without one containing \
                     the other, so their constraints are not hierarchic (paper § 4, Definition 1)",
                    a.name(),
                    b.name(),
                    a.file(),
                );
            }
        }
        // A file with a register past 64 (RISC-V CSRs) holds no value the
        // allocator constrains.
        let wide: BTreeSet<&str> = sets
            .iter()
            .filter(|(_, set)| set.last().is_some_and(|&index| index >= 64))
            .map(|(class, _)| class.file())
            .collect();
        let mut roots: Vec<(&'static str, u16, Set)> = Vec::new();
        for (class, set) in sets {
            if wide.contains(class.file()) {
                continue;
            }
            let set = set.iter().fold(0, |set, &index| set | bit(index));
            match roots
                .iter_mut()
                .find(|(file, width, _)| *file == class.file() && *width == class.group_width)
            {
                Some(root) => root.2 |= set,
                None => roots.push((class.file(), class.group_width, set)),
            }
        }
        Self { roots }
    }

    // The constrained instructions of `db`, by id: those with an operand or
    // result narrower than its file, through its slot's class or its pin.
    // Copies are not: they are the splits themselves.
    pub(super) fn constrained_ops(
        &self,
        context: &Context,
        db: &RegAllocDb,
        precolor: &HashMap<u32, PhysReg>,
    ) -> HashSet<u32> {
        let mut constrained = HashSet::new();
        for (ops, ids) in db.ops.iter().zip(&db.op_ids) {
            for (&op, &id) in ops.iter().zip(ids) {
                if self.is_constrained(context, op, precolor) {
                    constrained.insert(id);
                }
            }
        }
        constrained
    }

    fn is_constrained(
        &self,
        context: &Context,
        op_id: OpId,
        precolor: &HashMap<u32, PhysReg>,
    ) -> bool {
        let op = context.get_op(op_id);
        if op.is::<ParallelCopyOp>() || allocation_copy_endpoints(context, op_id).is_some() {
            return false;
        }
        let regs = op_regs(&op);
        // Registers an instruction names without reading or writing a value
        // constrain no value: the clobber rows keep what lives across off
        // them. A call still destroys the registers of what lives across it.
        if regs.uses.is_empty() && regs.defs.is_empty() && op.attr(CLOBBERS_ATTR).is_none() {
            return false;
        }
        let pinned = |value: &ValueId| precolor.contains_key(&value.number());
        if regs.uses.iter().chain(&regs.defs).any(pinned) {
            return true;
        }
        let named = |register: &PhysReg| {
            self.root(register.0)
                .is_some_and(|root| root & bit(register.1) != 0)
        };
        if regs.phys_uses.iter().chain(&regs.phys_defs).any(named) {
            return true;
        }
        reg_slots(&op).iter().any(|slot| {
            let RegSlot::Value(value) = slot.slot else {
                return false;
            };
            let own = value_class(context, value);
            let Some(class) = slot.port.class.or(own) else {
                return false;
            };
            let Some(root) = self.root(class) else {
                return false;
            };
            let narrowed = [Some(class), own]
                .into_iter()
                .flatten()
                .filter(|other| other.shares_view_with(class))
                .fold(root, |set, other| set & registers(other));
            narrowed != root
        })
    }

    fn root(&self, class: RegClassId) -> Option<Set> {
        self.roots
            .iter()
            .find(|(file, width, _)| *file == class.file() && *width == class.group_width)
            .map(|&(_, _, root)| root)
    }

    // The leaf of the hierarchy `vreg` sits at, and the root above it.
    fn constraint(
        &self,
        classes: &Classes,
        precolor: &HashMap<u32, PhysReg>,
        default_class: Option<RegClassId>,
        vreg: u32,
    ) -> Option<(Set, Set)> {
        let class = resolve_class(classes, precolor, default_class, vreg).ok()?;
        let root = self.root(class)?;
        let allowed = classes.allowed_indices.get(&vreg).map_or(root, |allowed| {
            allowed.iter().fold(0, |set, &index| set | bit(index))
        });
        let pinned = precolor.get(&vreg).map_or(root, |&(_, index)| bit(index));
        let set = root & allowed & pinned;
        (set != 0).then_some((set, root))
    }
}

fn registers(class: RegClassId) -> Set {
    class
        .registers
        .iter()
        .fold(0, |set, &index| set | bit(index))
}

fn bit(index: u16) -> Set {
    1u64.checked_shl(u32::from(index)).unwrap_or(0)
}

#[derive(Default)]
pub(super) struct Restricted {
    // Per value number, the registers left to it: its original constraint
    // intersected with its relaxed ones (§ 4.3).
    pub(super) sets: HashMap<u32, Set>,
    // Per constrained instruction, the values its parallel copy and it
    // define, in the order of § 4.3.
    pub(super) groups: Vec<Vec<u32>>,
}

// One value live before a constrained instruction; a pseudo operand has no
// value.
struct Input {
    value: Option<u32>,
    original: Set,
    dying: bool,
}

#[derive(Default)]
pub(super) struct Slots {
    pub(super) operands: Vec<u32>,
    pub(super) results: Vec<u32>,
    pub(super) live: Vec<u32>,
}

pub(super) fn slots(db: &RegAllocDb, ops: &HashSet<u32>) -> HashMap<u32, Slots> {
    let mut slots: HashMap<u32, Slots> = HashMap::new();
    let mut add = |table: u32, field: fn(&mut Slots) -> &mut Vec<u32>| {
        let table = db.table(table);
        for (&op, &value) in table.column(1).iter().zip(table.column(2)) {
            if ops.contains(&op) {
                field(slots.entry(op).or_default()).push(value);
            }
        }
    };
    add(db::USES, |slots| &mut slots.operands);
    add(db::DEFS, |slots| &mut slots.results);
    add(db::LIVE_AT, |slots| &mut slots.live);
    slots
}

#[allow(clippy::too_many_arguments)]
pub(super) fn restrict(
    hierarchy: &Hierarchy,
    context: &Context,
    db: &RegAllocDb,
    constrained: &HashSet<u32>,
    walk: &[usize],
    classes: &Classes,
    precolor: &HashMap<u32, PhysReg>,
    default_class: Option<RegClassId>,
) -> Result<Restricted, PassError> {
    let mut constraints: HashMap<u32, Option<(Set, Set)>> = HashMap::new();
    let mut constraint = |id: u32| {
        *constraints.entry(id).or_insert_with(|| {
            let vreg = db.value_of(id).number();
            hierarchy.constraint(classes, precolor, default_class, vreg)
        })
    };
    let slots = slots(db, constrained);

    let mut restricted = Restricted::default();
    for &block in walk {
        for (index, (op, &op_id)) in db.op_ids[block].iter().zip(&db.ops[block]).enumerate() {
            let Some(Slots {
                operands,
                results,
                live,
            }) = slots.get(op)
            else {
                continue;
            };
            // What the instruction and the parallel copy before it define:
            // the values the order of § 4.3 places there.
            let copy = index
                .checked_sub(1)
                .map(|before| db.ops[block][before])
                .filter(|&before| context.get_op(before).is::<ParallelCopyOp>());
            let mut local: HashSet<u32> = results.iter().copied().collect();
            if let Some(copy) = copy {
                let regs = op_regs(&context.get_op(copy));
                local.extend(regs.defs.iter().filter_map(|&value| db.value_id(value)));
            }
            let mut group = Vec::new();
            // Per root, the inputs and the results of that file.
            let mut files: Vec<File> = Vec::new();
            let inputs = operands
                .iter()
                .copied()
                .chain(live.iter().copied().filter(|v| !operands.contains(v)))
                .filter(|v| !results.contains(v));
            for value in inputs {
                if let Some((original, root)) = constraint(value) {
                    let dying = !live.contains(&value);
                    let at = file(&mut files, root);
                    files[at].1.push(Input {
                        value: Some(value),
                        original,
                        dying,
                    });
                }
            }
            for &value in results {
                if let Some((original, root)) = constraint(value) {
                    let at = file(&mut files, root);
                    files[at].2.push((Some(value), original));
                }
            }
            // A register the instruction reads by name is a dying operand
            // held there, and one it writes or clobbers a result.
            let regs = op_regs(&context.get_op(op_id));
            for (register, written) in regs
                .phys_uses
                .iter()
                .map(|r| (r, false))
                .chain(regs.phys_defs.iter().map(|r| (r, true)))
            {
                let Some(root) = hierarchy.root(register.0) else {
                    continue;
                };
                let set = root & bit(register.1);
                if set == 0 {
                    continue;
                }
                let at = file(&mut files, root);
                if written {
                    if !files[at].2.iter().any(|&(_, held)| held == set) {
                        files[at].2.push((None, set));
                    }
                } else if !files[at].1.iter().any(|input| input.original == set) {
                    files[at].1.push(Input {
                        value: None,
                        original: set,
                        dying: true,
                    });
                }
            }
            for (root, inputs, results) in files {
                let narrow = inputs.iter().any(|input| input.original != root)
                    || results.iter().any(|&(_, set)| set != root);
                if !narrow {
                    continue;
                }
                // A register read or written by name holds another value
                // there, which no node stands for.
                let named = |pseudo: Vec<Set>| pseudo.into_iter().fold(0, |set, held| set | held);
                let read = named(
                    inputs
                        .iter()
                        .filter(|i| i.value.is_none())
                        .map(|i| i.original)
                        .collect(),
                );
                let written = named(
                    results
                        .iter()
                        .filter(|r| r.0.is_none())
                        .map(|r| r.1)
                        .collect(),
                );
                let mut taken: HashMap<u32, Set> = HashMap::new();
                for input in inputs.iter().filter_map(|input| input.value) {
                    taken.insert(input, read);
                }
                for value in results.iter().filter_map(|result| result.0) {
                    taken.insert(value, written);
                }
                // Spilling left at most a file's registers live at every
                // instruction, and the constraints nest (Definition 1), so
                // Algorithm 2 pairs every constraint (paper § 4.2).
                let order = constrained_instruction(root, inputs, results).ok_or_else(|| {
                    PassError::InvalidRuleSet(format!(
                        "{} takes more registers of a file than it holds",
                        context.get_op(op_id).name().as_str()
                    ))
                })?;
                let mut keep = |value: u32, set: Set| {
                    let vreg = db.value_of(value).number();
                    *restricted.sets.entry(vreg).or_insert(set) &= set;
                };
                for &(value, set) in &order {
                    keep(value, set);
                }
                for (value, held) in taken {
                    keep(value, !held);
                }
                group.extend(
                    order
                        .iter()
                        .map(|&(value, _)| value)
                        .filter(|value| local.contains(value)),
                );
            }
            if !group.is_empty() {
                restricted.groups.push(group);
            }
        }
    }
    Ok(restricted)
}

// The root of one file at an instruction, the values live before it and
// its results.
type File = (Set, Vec<Input>, Vec<(Option<u32>, Set)>);

fn file(files: &mut Vec<File>, root: Set) -> usize {
    match files.iter().position(|file| file.0 == root) {
        Some(at) => at,
        None => {
            files.push((root, Vec::new(), Vec::new()));
            files.len() - 1
        }
    }
}

// The values at one constrained instruction of one file, in coloring order,
// each with the registers the PBQP leaves it. `None` where pressure exceeds
// the file or no restriction matches, the cases the spiller rules out.
fn constrained_instruction(
    root: Set,
    mut inputs: Vec<Input>,
    mut results: Vec<(Option<u32>, Set)>,
) -> Option<Vec<(u32, Set)>> {
    let registers = root.count_ones() as usize;
    let through = inputs.iter().filter(|input| !input.dying).count();
    if inputs.len() > registers || results.len() + through > registers {
        return None;
    }
    // Step 1: pseudo operands and results until pressure is the file's size.
    inputs.resize_with(registers, || Input {
        value: None,
        original: root,
        dying: true,
    });
    let mut outs: Vec<Set> = results.iter().map(|&(_, set)| set).collect();
    outs.resize(registers - through, root);

    // Step 2: Algorithm 2 over the dying operands and the results.
    let dying: Vec<usize> = (0..inputs.len()).filter(|&i| inputs[i].dying).collect();
    let mut ins: Vec<Set> = dying.iter().map(|&i| inputs[i].original).collect();
    restrict_inputs(&mut ins, &outs)?;
    let mut sets: Vec<Set> = inputs.iter().map(|input| input.original).collect();
    for (&i, set) in dying.iter().zip(ins) {
        sets[i] = set;
    }

    // Step 3: relax, then order (§ 4.3).
    let flags: Vec<bool> = inputs.iter().map(|input| input.dying).collect();
    let mut tree = Tree::new(root, &sets, &flags);
    tree.relax();
    let mut order = Vec::new();
    for node in tree.post_order() {
        let mut members = tree.nodes[node].inputs.clone();
        members.sort_by_key(|&i| inputs[i].original.count_ones());
        for i in members {
            if let Some(value) = inputs[i].value {
                order.push((value, inputs[i].original & tree.nodes[node].set));
            }
        }
    }
    results.sort_by_key(|&(_, set)| set.count_ones());
    order.extend(
        results
            .into_iter()
            .filter_map(|(value, set)| Some((value?, set))),
    );
    Some(order)
}

// Algorithm 2: pair a minimal constraint with a minimal partner on the other
// side that contains it, and hand a result's constraint to its operand.
// `None` where a constraint finds no partner.
fn restrict_inputs(ins: &mut [Set], outs: &[Set]) -> Option<()> {
    let mut open: Vec<(bool, usize)> = (0..ins.len())
        .map(|i| (true, i))
        .chain((0..outs.len()).map(|i| (false, i)))
        .collect();
    while !open.is_empty() {
        let set = |&(input, i): &(bool, usize)| if input { ins[i] } else { outs[i] };
        let min = *open
            .iter()
            .find(|&c| !open.iter().any(|d| strict_subset(set(d), set(c))))?;
        let min_set = set(&min);
        let partner = *open
            .iter()
            .filter(|d| d.0 != min.0 && min_set & !set(d) == 0)
            .min_by_key(|d| set(d).count_ones())?;
        if partner.0 {
            ins[partner.1] = min_set;
        }
        open.retain(|&c| c != min && c != partner);
    }
    Some(())
}

fn strict_subset(a: Set, b: Set) -> bool {
    a != b && a & !b == 0
}

// A constraint of the operand hierarchy and the operands it holds.
struct Node {
    set: Set,
    parent: usize,
    inputs: Vec<usize>,
    alive: bool,
}

// The hierarchy of the operand constraints of one instruction; node 0 is
// the root, the whole file.
struct Tree<'a> {
    nodes: Vec<Node>,
    dying: &'a [bool],
}

impl<'a> Tree<'a> {
    fn new(root: Set, sets: &[Set], dying: &'a [bool]) -> Self {
        let mut nodes = vec![Node {
            set: root,
            parent: 0,
            inputs: Vec::new(),
            alive: true,
        }];
        for (input, &set) in sets.iter().enumerate() {
            match nodes.iter().position(|node| node.set == set) {
                Some(at) => nodes[at].inputs.push(input),
                None => nodes.push(Node {
                    set,
                    parent: 0,
                    inputs: vec![input],
                    alive: true,
                }),
            }
        }
        for at in 1..nodes.len() {
            nodes[at].parent = (0..nodes.len())
                .filter(|&other| strict_subset(nodes[at].set, nodes[other].set))
                .min_by_key(|&other| nodes[other].set.count_ones())
                .unwrap_or(0);
        }
        Self { nodes, dying }
    }

    // Apply the rules of Figure 4 greedily, left to right, until none
    // applies. Every rule strictly widens a dying operand's constraint,
    // narrows a live-through one's, or removes a node, so this ends.
    fn relax(&mut self) {
        while self.merge_saturated() || self.restrict_live_through() || self.lift_dying() {}
    }

    fn children(&self, node: usize) -> Vec<usize> {
        (1..self.nodes.len())
            .filter(|&n| self.nodes[n].alive && self.nodes[n].parent == node)
            .collect()
    }

    fn within(&self, mut node: usize, ancestor: usize) -> bool {
        while node != ancestor {
            if node == 0 {
                return false;
            }
            node = self.nodes[node].parent;
        }
        true
    }

    fn saturated(&self, node: usize) -> bool {
        let held: usize = (0..self.nodes.len())
            .filter(|&n| self.nodes[n].alive && self.within(n, node))
            .map(|n| self.nodes[n].inputs.len())
            .sum();
        self.nodes[node].set.count_ones() as usize == held
    }

    fn all_dying(&self, node: usize) -> bool {
        self.nodes[node].inputs.iter().all(|&i| self.dying[i])
    }

    // Figure 4a: two saturated sibling constraints of dying operands become
    // their union.
    fn merge_saturated(&mut self) -> bool {
        for a in 1..self.nodes.len() {
            if !self.nodes[a].alive || !self.saturated(a) || !self.all_dying(a) {
                continue;
            }
            let parent = self.nodes[a].parent;
            let partner = self
                .children(parent)
                .into_iter()
                .find(|&b| b != a && self.saturated(b) && self.all_dying(b));
            let Some(b) = partner else {
                continue;
            };
            let inputs = std::mem::take(&mut self.nodes[b].inputs);
            self.nodes[a].inputs.extend(inputs);
            self.nodes[a].set |= self.nodes[b].set;
            self.remove(b);
            if self.nodes[a].set == self.nodes[parent].set {
                let inputs = std::mem::take(&mut self.nodes[a].inputs);
                self.nodes[parent].inputs.extend(inputs);
                self.remove(a);
            }
            return true;
        }
        false
    }

    // Figure 4b: the live-through operands of a constraint give up the
    // registers of a saturated child.
    fn restrict_live_through(&mut self) -> bool {
        for upper in 0..self.nodes.len() {
            if !self.nodes[upper].alive || self.all_dying(upper) {
                continue;
            }
            let Some(full) = self
                .children(upper)
                .into_iter()
                .find(|&child| self.saturated(child))
            else {
                continue;
            };
            let (dying, through): (Vec<usize>, Vec<usize>) = self.nodes[upper]
                .inputs
                .iter()
                .partition(|&&i| self.dying[i]);
            let set = self.nodes[upper].set & !self.nodes[full].set;
            let others: Vec<usize> = self
                .children(upper)
                .into_iter()
                .filter(|&child| child != full)
                .collect();
            match others.iter().find(|&&child| self.nodes[child].set == set) {
                Some(&same) => self.nodes[same].inputs.extend(through),
                None => {
                    let rest = self.nodes.len();
                    for child in others {
                        self.nodes[child].parent = rest;
                    }
                    self.nodes.push(Node {
                        set,
                        parent: upper,
                        inputs: through,
                        alive: true,
                    });
                }
            }
            self.nodes[upper].inputs = dying;
            if upper != 0 && self.nodes[upper].inputs.is_empty() {
                self.remove(upper);
            }
            return true;
        }
        false
    }

    // Figure 4c: dying operands move up to a parent that holds no
    // live-through operand.
    fn lift_dying(&mut self) -> bool {
        for lower in 1..self.nodes.len() {
            let parent = self.nodes[lower].parent;
            if !self.nodes[lower].alive || !self.all_dying(parent) {
                continue;
            }
            let (dying, through): (Vec<usize>, Vec<usize>) = self.nodes[lower]
                .inputs
                .iter()
                .partition(|&&i| self.dying[i]);
            if dying.is_empty() {
                continue;
            }
            self.nodes[parent].inputs.extend(dying);
            self.nodes[lower].inputs = through;
            if self.nodes[lower].inputs.is_empty() {
                self.remove(lower);
            }
            return true;
        }
        false
    }

    // Drop `node`, its children joining its parent.
    fn remove(&mut self, node: usize) {
        let parent = self.nodes[node].parent;
        for child in self.children(node) {
            self.nodes[child].parent = parent;
        }
        self.nodes[node].alive = false;
    }

    fn post_order(&self) -> Vec<usize> {
        let mut order = Vec::new();
        let mut stack = vec![(0, false)];
        while let Some((node, done)) = stack.pop() {
            if done {
                order.push(node);
            } else {
                stack.push((node, true));
                stack.extend(
                    self.children(node)
                        .into_iter()
                        .rev()
                        .map(|child| (child, false)),
                );
            }
        }
        order
    }
}

// The coloring order of the whole graph (§ 4.3): `peo`, where the values
// at each constrained instruction take the order section 4 gives them.
pub(super) fn color_order(restricted: &Restricted, peo: &[u32]) -> Vec<u32> {
    let mut group_of: HashMap<u32, usize> = HashMap::new();
    for (group, values) in restricted.groups.iter().enumerate() {
        for &value in values {
            group_of.entry(value).or_insert(group);
        }
    }
    let mut placed: HashSet<u32> = HashSet::with_capacity(peo.len());
    let mut order = Vec::with_capacity(peo.len());
    for &value in peo {
        if let Some(&group) = group_of.get(&value) {
            order.extend(
                restricted.groups[group]
                    .iter()
                    .filter(|&&v| placed.insert(v)),
            );
        }
        if placed.insert(value) {
            order.push(value);
        }
    }
    order
}
