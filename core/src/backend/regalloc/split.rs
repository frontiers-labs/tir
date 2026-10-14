//! The split before every constrained instruction (paper § 4, after Hack's
//! thesis): one parallel copy of every value live there, so what the
//! instruction constrains is a range of its own. Later reads take the copy's
//! name, and a block where two names of one value meet takes it as a
//! parameter, so the function stays in strict SSA form.

use std::collections::{HashMap, HashSet};

use tir::attributes::AttributeValue;
use tir::{BlockId, Context, OpId, PassError, Terminator, ValueId};

use super::constraints;
use super::db::{self, RegAllocDb};
use super::{RegisterAllocationPass, op_ref_in};
use crate::backend::liveness::PhysReg;
use crate::backend::machine_cfg::Dominance;
use crate::backend::registers::value_class;
use crate::backend::{ParallelCopyOpBuilder, VirtualBranchOp};

// A constrained instruction and the values live before it, by database id.
struct Site {
    block: usize,
    index: usize,
    op: OpId,
    values: Vec<u32>,
}

// Insert the parallel copies and rename every later read. Returns the
// copies, for the database to seed their pairs; pins on operands move to
// their copies.
#[allow(clippy::too_many_arguments)]
pub(super) fn split(
    pass: &mut RegisterAllocationPass,
    context: &Context,
    blocks: &[BlockId],
    successors: &HashMap<BlockId, Vec<BlockId>>,
    dominance: &Dominance,
    db: &mut RegAllocDb,
    constrained: &HashSet<u32>,
    precolor: &mut HashMap<u32, PhysReg>,
) -> Result<Vec<(OpId, BlockId)>, PassError> {
    db.saturate(&pass.rules.spill, &mut pass.caches);
    if constrained.is_empty() {
        return Ok(Vec::new());
    }
    let db = &*db;
    let mut sites = sites(context, db, &constraints::slots(db, constrained), precolor);
    let index: HashMap<BlockId, usize> = blocks.iter().enumerate().map(|(i, &b)| (b, i)).collect();
    let preds = predecessors(blocks, successors, &index);
    let (meet, whole) = meeting_points(context, blocks, &preds, dominance, db, &sites);
    for site in &mut sites {
        site.values.retain(|value| !whole.contains(value));
    }
    sites.retain(|site| !site.values.is_empty());
    if sites.is_empty() {
        return Ok(Vec::new());
    }

    let mut copies = Vec::new();
    let mut names: HashMap<u32, Vec<(usize, u32, ValueId)>> = HashMap::new();
    for site in &sites {
        let copy = insert_copy(context, db, site)?;
        let results = context.get_op(copy).results().to_vec();
        for (&value, &result) in site.values.iter().zip(&results) {
            let named = names.entry(value).or_default();
            named.push((site.block, db::before(site.index), result));
            if let Some(register) = precolor.remove(&db.value_of(value).number()) {
                precolor.insert(result.number(), register);
            }
        }
        copies.push((copy, blocks[site.block]));
    }

    // Parameters and the branch arguments passing them go in one order.
    let mut met: Vec<(usize, u32)> = meet
        .iter()
        .flat_map(|(&value, at)| at.iter().map(move |&b| (b, value)))
        .collect();
    met.sort_unstable();
    let mut params: HashMap<(usize, u32), ValueId> = HashMap::new();
    for &(b, value) in &met {
        let ty = context.get_value(db.value_of(value)).ty();
        params.insert(
            (b, value),
            context.append_block_argument(blocks[b], ty).id(),
        );
    }
    let mut reach = Reach {
        names: &names,
        params: &params,
        dominance,
        db,
        end: HashMap::new(),
    };
    rename(context, db, &sites, &copies, &mut reach);
    for &(b, value) in &met {
        for &pred in &preds[b] {
            let arg = reach.end(value, pred);
            let branch = branch_to(context, blocks[pred], blocks[b]).ok_or_else(|| {
                PassError::InvalidRuleSet(format!("block {pred} has no asm.vbr to block {b}"))
            })?;
            let op = context.get_op(branch);
            let (at, end) = (op.value_operands().len(), op.operands().len());
            context.insert_operand_at(branch, at, arg, end);
        }
    }
    Ok(copies)
}

// The constrained instructions and the register values live before each.
// A pinned value read there moves its pin to the copy; one pinned across
// stays whole, as the register it holds is fixed anyway.
fn sites(
    context: &Context,
    db: &RegAllocDb,
    slots: &HashMap<u32, constraints::Slots>,
    precolor: &HashMap<u32, PhysReg>,
) -> Vec<Site> {
    let params: HashSet<u32> = db.table(db::PARAM).column(2).iter().copied().collect();
    let pinned = |value: u32| precolor.contains_key(&db.value_of(value).number());
    let mut sites = Vec::new();
    for (block, ids) in db.op_ids.iter().enumerate() {
        for (index, id) in ids.iter().enumerate() {
            let Some(slots) = slots.get(id) else {
                continue;
            };
            let operands = slots
                .operands
                .iter()
                .copied()
                .filter(|&value| !pinned(value) || !params.contains(&value));
            let through = slots.live.iter().copied().filter(|value| {
                !slots.results.contains(value) && !slots.operands.contains(value) && !pinned(*value)
            });
            let values: Vec<u32> = operands
                .chain(through)
                .filter(|&value| value_class(context, db.value_of(value)).is_some())
                .collect();
            if !values.is_empty() {
                sites.push(Site {
                    block,
                    index,
                    op: db.ops[block][index],
                    values,
                });
            }
        }
    }
    sites
}

// Per split value, the blocks where its names meet: the iterated dominance
// frontier of the blocks writing it, where it is live in. A conditional
// branch carries no arguments, so names may not meet where one enters; a
// value that would is left whole (the second set).
fn meeting_points(
    context: &Context,
    blocks: &[BlockId],
    preds: &[Vec<usize>],
    dominance: &Dominance,
    db: &RegAllocDb,
    sites: &[Site],
) -> (HashMap<u32, Vec<usize>>, HashSet<u32>) {
    let mut writes: HashMap<u32, Vec<usize>> = HashMap::new();
    for site in sites {
        for &value in &site.values {
            writes.entry(value).or_default().push(site.block);
        }
    }
    let block_of: HashMap<u32, usize> = db
        .block_ids
        .iter()
        .enumerate()
        .map(|(index, &id)| (id, index))
        .collect();
    let mut op_block: HashMap<u32, usize> = HashMap::new();
    for (block, ids) in db.op_ids.iter().enumerate() {
        op_block.extend(ids.iter().map(|&id| (id, block)));
    }
    let rows = |table: u32| {
        let table = db.table(table);
        let pairs: Vec<(u32, u32)> = table
            .column(1)
            .iter()
            .copied()
            .zip(table.column(2).iter().copied())
            .collect();
        pairs
    };
    for (op, value) in rows(db::DEFS) {
        if let Some(written) = writes.get_mut(&value) {
            written.push(op_block[&op]);
        }
    }
    for (block, value) in rows(db::PARAM) {
        if let Some(written) = writes.get_mut(&value) {
            written.push(block_of[&block]);
        }
    }
    let livein: HashSet<(usize, u32)> = rows(db::LIVEIN)
        .into_iter()
        .filter(|(_, value)| writes.contains_key(value))
        .map(|(block, value)| (block_of[&block], value))
        .collect();
    let frontier = frontiers(preds, dominance);
    let branches: Vec<bool> = (0..blocks.len())
        .map(|b| entered_by_branches(context, blocks, &preds[b], blocks[b]))
        .collect();
    let mut meet = HashMap::new();
    let mut whole = HashSet::new();
    for (value, written) in writes {
        let at: Vec<usize> = iterated_frontier(&frontier, written)
            .into_iter()
            .filter(|&b| livein.contains(&(b, value)))
            .collect();
        if at.iter().all(|&b| branches[b]) {
            meet.insert(value, at);
        } else {
            whole.insert(value);
        }
    }
    (meet, whole)
}

fn insert_copy(context: &Context, db: &RegAllocDb, site: &Site) -> Result<OpId, PassError> {
    let sources: Vec<ValueId> = site
        .values
        .iter()
        .map(|&value| db.value_of(value))
        .collect();
    let types = sources
        .iter()
        .map(|&value| context.get_value(value).ty())
        .collect();
    let copy = ParallelCopyOpBuilder::new(context)
        .sources(sources)
        .result_types(types)
        .build();
    context.insert_op_before(&op_ref_in(context, site.op), &copy)?;
    Ok(tir::Operation::id(&copy))
}

// Every read of a split value, the copies' own included, takes the name
// that reaches it.
fn rename(
    context: &Context,
    db: &RegAllocDb,
    sites: &[Site],
    copies: &[(OpId, BlockId)],
    reach: &mut Reach<'_>,
) {
    let mut renames: Vec<(OpId, usize, ValueId)> = Vec::new();
    for (block, ops) in db.ops.iter().enumerate() {
        for (index, &op) in ops.iter().enumerate() {
            let operands = context.get_op(op).operands().to_vec();
            for (operand, &value) in operands.iter().enumerate() {
                let Some(value) = db.value_id(value).filter(|v| reach.names.contains_key(v)) else {
                    continue;
                };
                let name = reach.at(value, block, db::point(index));
                if name != db.value_of(value) {
                    renames.push((op, operand, name));
                }
            }
        }
    }
    for (site, &(copy, _)) in sites.iter().zip(copies) {
        for (operand, &value) in site.values.iter().enumerate() {
            let name = reach.at(value, site.block, db::before(site.index));
            if name != db.value_of(value) {
                renames.push((copy, operand, name));
            }
        }
    }
    for (op, operand, name) in renames {
        context.set_op_operand(op, operand, name);
    }
}

// The name of one value at a point: the last copy before it in its block,
// or the name the block enters with.
struct Reach<'a> {
    names: &'a HashMap<u32, Vec<(usize, u32, ValueId)>>,
    params: &'a HashMap<(usize, u32), ValueId>,
    dominance: &'a Dominance,
    db: &'a RegAllocDb,
    end: HashMap<(usize, u32), ValueId>,
}

impl Reach<'_> {
    fn at(&mut self, value: u32, block: usize, point: u32) -> ValueId {
        let written = self.names[&value]
            .iter()
            .filter(|&&(b, at, _)| b == block && at < point)
            .max_by_key(|&&(_, at, _)| at);
        match written {
            Some(&(_, _, name)) => name,
            None => self.entry(value, block),
        }
    }

    fn entry(&mut self, value: u32, block: usize) -> ValueId {
        if let Some(&param) = self.params.get(&(block, value)) {
            return param;
        }
        match self.dominance.idom(block) {
            Some(idom) if idom != block => self.end(value, idom),
            _ => self.db.value_of(value),
        }
    }

    fn end(&mut self, value: u32, block: usize) -> ValueId {
        if let Some(&name) = self.end.get(&(block, value)) {
            return name;
        }
        let name = self.at(value, block, u32::MAX);
        self.end.insert((block, value), name);
        name
    }
}

fn predecessors(
    blocks: &[BlockId],
    successors: &HashMap<BlockId, Vec<BlockId>>,
    index: &HashMap<BlockId, usize>,
) -> Vec<Vec<usize>> {
    let mut preds = vec![Vec::new(); blocks.len()];
    for (b, block) in blocks.iter().enumerate() {
        for succ in successors.get(block).into_iter().flatten() {
            preds[index[succ]].push(b);
        }
    }
    preds
}

// The dominance frontier of every block (Cytron et al.).
fn frontiers(preds: &[Vec<usize>], dominance: &Dominance) -> Vec<Vec<usize>> {
    let mut frontier = vec![Vec::new(); preds.len()];
    for (b, incoming) in preds.iter().enumerate() {
        if incoming.len() < 2 {
            continue;
        }
        let Some(idom) = dominance.idom(b) else {
            continue;
        };
        for &pred in incoming {
            let mut runner = pred;
            while runner != idom {
                if !frontier[runner].contains(&b) {
                    frontier[runner].push(b);
                }
                match dominance.idom(runner) {
                    Some(up) if up != runner => runner = up,
                    _ => break,
                }
            }
        }
    }
    frontier
}

fn iterated_frontier(frontier: &[Vec<usize>], mut work: Vec<usize>) -> Vec<usize> {
    let mut seen = HashSet::new();
    let mut meet = Vec::new();
    while let Some(b) = work.pop() {
        for &f in &frontier[b] {
            if seen.insert(f) {
                meet.push(f);
                work.push(f);
            }
        }
    }
    meet
}

// Whether every predecessor enters `block` through an `asm.vbr`, which can
// pass it arguments.
fn entered_by_branches(
    context: &Context,
    blocks: &[BlockId],
    preds: &[usize],
    block: BlockId,
) -> bool {
    preds.iter().all(|&pred| {
        context
            .get_block(blocks[pred])
            .op_ids()
            .into_iter()
            .all(|op| {
                let op = context.get_op(op);
                let branch = op.is::<VirtualBranchOp>();
                let enters = op
                    .as_interface::<dyn Terminator>()
                    .is_some_and(|term| term.successors().contains(&block));
                !enters || branch
            })
    })
}

fn branch_to(context: &Context, pred: BlockId, block: BlockId) -> Option<OpId> {
    context
        .get_block(pred)
        .op_ids()
        .into_iter()
        .rev()
        .find(|&op| {
            let op = context.get_op(op);
            op.is::<VirtualBranchOp>()
                && matches!(op.attr("dest"), Some(AttributeValue::Block(dest)) if dest == block)
        })
}
