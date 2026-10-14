//! The spiller (paper § 2.1: spilling is decoupled from assignment): Belady
//! per block, after Hack's thesis (paper ref. 11).
//!
//! Blocks are visited in reverse postorder, with next-use distances taken
//! across blocks (Braun & Hack 2009). A loop header starts with the values its
//! loop reads, then what lives through the loop while the loop leaves room; any
//! other block with what its predecessors ended with in registers. An
//! operation that does not fit evicts the value read farthest ahead. An evicted value is stored once, after its definition, and reloaded
//! into a fresh register where it is read again; a constant is replayed
//! instead. A block whose predecessors hold a value under different names
//! takes it as a parameter, so the function stays in strict SSA form.

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use smallvec::SmallVec;
use tir::attributes::AttributeValue;
use tir::{BlockId, Context, OpId, PassError, Terminator, ValueId};

use super::db::{self, RegAllocDb};
use super::{
    FramePlan, RegClassId, RegisterAllocationPass, SlotChain, allocation_order, op_ref_in,
};
use crate::backend::VirtualBranchOp;
use crate::backend::liveness::{Classes, PhysReg, op_regs};
use crate::backend::machine_cfg::{Dominance, Loops};
use crate::backend::registers::{fresh_reg, value_class};

// Spill until every point of the function fits its register files, as one
// batch of reloads and stores. Reports whether it changed the function, so
// the caller reads it again.
#[allow(clippy::too_many_arguments)]
pub(super) fn spill(
    pass: &mut RegisterAllocationPass,
    context: &Context,
    blocks: &[BlockId],
    dominance: &Dominance,
    classes: &Classes,
    remat: &HashMap<u32, OpId>,
    db: &mut RegAllocDb,
    frame: &mut FramePlan,
) -> Result<bool, PassError> {
    let function = Function::read(pass, context, blocks, dominance, classes, remat, db);
    if function.fits() {
        return Ok(false);
    }
    let mut spiller = Spiller::new(&function, db, context);
    for block in dominance.reverse_postorder() {
        spiller.block(block)?;
    }
    spiller.join(blocks)?;
    spiller.emit(pass, blocks, frame)?;
    Ok(true)
}

// The order of what goes before one op: the store after the previous op's
// definition, the reloads the op reads from a slot its edge fills or of what
// the edge stores, the edge stores of memory parameters from registers, those
// through a reloaded register, then the other reloads the op reads.
const DEF_STORE: u8 = 0;
const EDGE_LOAD: u8 = 1;
const EDGE_STORE: u8 = 2;
const COPY: u8 = 3;
const RELOAD: u8 = 4;

// The length of an edge leaving a loop: a read past the loop is farther
// than any read in it (Braun & Hack, § 4.1).
const LOOP_EXIT: u64 = 100_000;
// A live-out value no successor reads: an argument the branch passes.
const FAR: u64 = 1 << 40;
// No read follows and the value is not live out.
const DEAD: u64 = u64::MAX;

// What the database says about the function, by block index. Values are
// database ids.
struct Function {
    capacity: Vec<u32>,
    shapes: HashMap<u32, Shape>,
    blocks: Vec<Block>,
    pinned: HashSet<u32>,
    remat: HashMap<u32, OpId>,
    loops: Loops,
    // Parameters left out of their block's registers: their slot is filled
    // on every incoming edge, and every use reloads them.
    memory: HashSet<u32>,
    // Memory parameters nothing reads.
    dead: HashSet<u32>,
}

#[derive(Clone, Copy)]
struct Shape {
    file: usize,
    width: u32,
    class: RegClassId,
}

#[derive(Default)]
struct Block {
    ops: Vec<Op>,
    params: Vec<u32>,
    livein: Vec<u32>,
    liveout: HashSet<u32>,
    // Per live-out value, the distance from the block's end to its next read.
    next_out: HashMap<u32, u64>,
    // The values read in the block or, for a loop header, in its loop.
    reads: HashSet<u32>,
    preds: Vec<usize>,
    succs: Vec<usize>,
    // Every predecessor reaches it through an `asm.vbr`, which can pass it
    // arguments.
    phis: bool,
    // Per file, the units left on entry for values the block does not read.
    room: Vec<u32>,
}

impl Block {
    // The distance from the block's start to the next read of `value` past
    // the block.
    fn past(&self, value: u32) -> u64 {
        if !self.liveout.contains(&value) {
            return DEAD;
        }
        self.ops.len() as u64 + self.next_out.get(&value).copied().unwrap_or(FAR)
    }
}

impl Op {
    // The reads Belady times. A constant an edge passes is replayed on the
    // edge, so it needs no register ahead of the branch.
    fn timed<'a>(&'a self, remat: &'a HashMap<u32, OpId>) -> impl Iterator<Item = u32> + 'a {
        self.uses.iter().copied().filter(move |value| {
            !remat.contains_key(value) || !self.args.iter().any(|&(_, arg, _)| arg == *value)
        })
    }
}

struct Op {
    id: OpId,
    uses: SmallVec<[u32; 4]>,
    defs: SmallVec<[u32; 2]>,
    // Per file, the width of the values in a register after the op, and of
    // those with its unread results.
    live: SmallVec<[u32; 2]>,
    written: SmallVec<[u32; 2]>,
    // Per file, the width of the values in a register across the op, which
    // it neither reads last nor writes.
    through: SmallVec<[u32; 2]>,
    // For an `asm.vbr`: `(parameter, argument, operand position)`.
    args: SmallVec<[(u32, u32, usize); 4]>,
    // The allocatable `(file, unit)`s the op writes and reads by name.
    fixed_defs: SmallVec<[(usize, u16); 2]>,
    fixed_uses: SmallVec<[(usize, u16); 2]>,
    // The units of the pinned values the op reads last or writes.
    pinned_units: SmallVec<[(usize, u16); 2]>,
}

// Per file, the allocatable units registers named directly take at one op:
// before it, after it, and from what lives across it, which also loses what
// the op clobbers. At the op itself, a register it reads or writes by name
// or through a pin pairs only with a result or operand allowed to take it
// (paper § 4.1), so those registers count once each beside what lives
// across.
#[derive(Clone)]
struct Fixed {
    before: SmallVec<[u32; 2]>,
    after: SmallVec<[u32; 2]>,
    across: SmallVec<[u32; 2]>,
    at: SmallVec<[u32; 2]>,
}

fn fixed(ops: &[Op], files: usize) -> Vec<Fixed> {
    let count = |units: &[(usize, u16)]| {
        let mut per_file: SmallVec<[u32; 2]> = SmallVec::from_elem(0, files);
        for &(file, _) in units {
            per_file[file] += 1;
        }
        per_file
    };
    let mut out = vec![
        Fixed {
            before: SmallVec::new(),
            after: SmallVec::new(),
            across: SmallVec::new(),
            at: SmallVec::new(),
        };
        ops.len()
    ];
    let mut live: Vec<(usize, u16)> = Vec::new();
    for (index, op) in ops.iter().enumerate().rev() {
        out[index].after = count(&live);
        let mut across = live.clone();
        across.extend(op.fixed_defs.iter().filter(|unit| !live.contains(unit)));
        out[index].across = count(&across);
        let mut at = across;
        for unit in op.fixed_uses.iter().chain(&op.pinned_units) {
            if !at.contains(unit) {
                at.push(*unit);
            }
        }
        out[index].at = count(&at);
        live.retain(|unit| !op.fixed_defs.contains(unit));
        for unit in &op.fixed_uses {
            if !live.contains(unit) {
                live.push(*unit);
            }
        }
        out[index].before = count(&live);
    }
    out
}

impl Function {
    fn read(
        pass: &RegisterAllocationPass,
        context: &Context,
        blocks: &[BlockId],
        dominance: &Dominance,
        classes: &Classes,
        remat: &HashMap<u32, OpId>,
        db: &RegAllocDb,
    ) -> Self {
        let block_index: HashMap<u32, usize> = db
            .block_ids
            .iter()
            .enumerate()
            .map(|(index, &id)| (id, index))
            .collect();
        let mut position: HashMap<u32, (usize, usize)> = HashMap::new();
        let mut out: Vec<Block> = Vec::with_capacity(blocks.len());
        for (block, keys) in db.op_ids.iter().enumerate() {
            let mut ops = Vec::with_capacity(keys.len());
            for (index, (&key, &id)) in keys.iter().zip(&db.ops[block]).enumerate() {
                position.insert(key, (block, index));
                ops.push(Op {
                    id,
                    uses: SmallVec::new(),
                    defs: SmallVec::new(),
                    live: SmallVec::new(),
                    written: SmallVec::new(),
                    through: SmallVec::new(),
                    args: SmallVec::new(),
                    fixed_defs: SmallVec::new(),
                    fixed_uses: SmallVec::new(),
                    pinned_units: SmallVec::new(),
                });
            }
            out.push(Block {
                ops,
                params: db.params[block].clone(),
                phis: true,
                ..Block::default()
            });
        }
        let rows = |slot: u32| {
            let table = db.table(slot);
            (table.column(1).to_vec(), table.column(2).to_vec())
        };
        let (ops, values) = rows(db::USES);
        for (op, value) in ops.into_iter().zip(values) {
            let (block, index) = position[&op];
            out[block].ops[index].uses.push(value);
        }
        let (ops, values) = rows(db::DEFS);
        for (op, value) in ops.into_iter().zip(values) {
            let (block, index) = position[&op];
            out[block].ops[index].defs.push(value);
        }
        let (ids, values) = rows(db::LIVEIN);
        for (block, value) in ids.into_iter().zip(values) {
            out[block_index[&block]].livein.push(value);
        }
        let (ids, values) = rows(db::LIVEOUT);
        for (block, value) in ids.into_iter().zip(values) {
            out[block_index[&block]].liveout.insert(value);
        }
        let pinned: HashSet<u32> = db.table(db::PIN).column(1).iter().copied().collect();

        let info = pass.target.register_info();
        let default_class = info.default_integer_class(pass.abi);
        let mut files: Vec<(&'static str, HashSet<u16>)> = Vec::new();
        let mut seen: HashSet<RegClassId> = HashSet::new();
        let mut shapes = HashMap::new();
        let named = out.iter().flat_map(|block| {
            let ops = block.ops.iter();
            block
                .params
                .iter()
                .chain(ops.flat_map(|op| op.uses.iter().chain(&op.defs)))
        });
        for &id in named {
            if shapes.contains_key(&id) {
                continue;
            }
            let value = db.value_of(id);
            let Some(class) = classes
                .vreg_class
                .get(&value.number())
                .copied()
                .or_else(|| value_class(context, value))
                .or(default_class)
            else {
                continue;
            };
            let file = match files.iter().position(|(name, _)| *name == class.file()) {
                Some(file) => file,
                None => {
                    files.push((class.file(), HashSet::new()));
                    files.len() - 1
                }
            };
            if seen.insert(class) {
                for register in allocation_order(pass.abi, class) {
                    let (_, start, width) = register.0.span(register.1);
                    files[file].1.extend(start..start + width);
                }
            }
            let width = u32::from(class.group_width.max(1));
            shapes.insert(id, Shape { file, width, class });
        }
        let capacity: Vec<u32> = files.iter().map(|(_, units)| units.len() as u32).collect();

        let units = |(class, index): PhysReg| -> Vec<(usize, u16)> {
            let (name, start, width) = class.span(index);
            let Some(file) = files.iter().position(|(file, _)| *file == name) else {
                return Vec::new();
            };
            (start..start + width)
                .filter(|unit| files[file].1.contains(unit))
                .map(|unit| (file, unit))
                .collect()
        };
        let clobbers = db.table(db::CLOBBER);
        for (&op, &register) in clobbers.column(1).iter().zip(clobbers.tags()) {
            let (block, index) = position[&op];
            let units = units(db.registers[register as usize]);
            out[block].ops[index].fixed_defs.extend(units);
        }
        for op in out.iter_mut().flat_map(|block| &mut block.ops) {
            for register in op_regs(&context.get_op(op.id)).phys_uses {
                op.fixed_uses.extend(units(register));
            }
        }
        let pins = db.table(db::PIN);
        let pinned_units: HashMap<u32, Vec<(usize, u16)>> = pins
            .column(1)
            .iter()
            .zip(pins.tags())
            .map(|(&value, &register)| (value, units(db.registers[register as usize])))
            .collect();
        // Per block and value, the ops its range holds the value after; per
        // block, how the widths held change from one op to the next.
        let file_count = capacity.len();
        let mut held: HashMap<(usize, u32), Range<usize>> = HashMap::new();
        let mut steps: Vec<Vec<i64>> = out
            .iter()
            .map(|block| vec![0; (block.ops.len() + 1) * file_count])
            .collect();
        let ranges = db.table(db::RANGE);
        let keys = ranges.column(1).iter().zip(ranges.column(2));
        for ((block, &value), &range) in keys.zip(ranges.values()) {
            let (Some(start), Some(end)) =
                (db.db.read(db::START, range), db.db.read(db::END, range))
            else {
                continue;
            };
            let block = block_index[block];
            let after = db::held_after(start as i64, end as i64, out[block].ops.len());
            if let Some(shape) = shapes.get(&value) {
                let width = i64::from(shape.width);
                steps[block][after.start * file_count + shape.file] += width;
                steps[block][after.end * file_count + shape.file] -= width;
            }
            held.insert((block, value), after);
        }
        for (b, (block, steps)) in out.iter_mut().zip(&steps).enumerate() {
            let mut live = vec![0i64; file_count];
            for (index, op) in block.ops.iter_mut().enumerate() {
                for (file, width) in live.iter_mut().enumerate() {
                    *width += steps[index * file_count + file];
                }
                op.live = live.iter().map(|&width| width as u32).collect();
                op.written = op.live.clone();
                op.through = op.live.clone();
                let held_after = |value: &u32| {
                    held.get(&(b, *value))
                        .is_some_and(|after| after.contains(&index))
                };
                for def in &op.defs {
                    if let Some(shape) = shapes.get(def) {
                        if held_after(def) {
                            op.through[shape.file] -= shape.width;
                        } else {
                            op.written[shape.file] += shape.width;
                        }
                    }
                }
                let read_last = op.uses.iter().filter(|value| !held_after(value));
                for value in read_last.chain(&op.defs) {
                    op.pinned_units
                        .extend(pinned_units.get(value).into_iter().flatten().copied());
                }
            }
        }

        read_edges(context, blocks, db, &mut out);

        let remat = remat
            .iter()
            .filter_map(|(&value, &def)| Some((db.value_id(ValueId::from_number(value))?, def)))
            .collect();
        let mut function = Self {
            capacity,
            shapes,
            blocks: out,
            pinned,
            remat,
            loops: dominance.loops(),
            memory: HashSet::new(),
            dead: HashSet::new(),
        };
        function.next_uses(&dominance.reverse_postorder());
        function.choose_memory_phis();
        function
    }

    fn next_uses(&mut self, order: &[usize]) {
        // Braun & Hack § 4.1: liveness with distances, joined by minimum.
        let Self {
            blocks,
            loops,
            remat,
            ..
        } = self;
        let mut first: Vec<HashMap<u32, u64>> = Vec::with_capacity(blocks.len());
        let mut reads: Vec<HashSet<u32>> = vec![HashSet::new(); blocks.len()];
        for (b, block) in blocks.iter().enumerate() {
            let mut at = HashMap::new();
            for (index, op) in block.ops.iter().enumerate().rev() {
                for value in op.timed(remat) {
                    at.insert(value, index as u64);
                }
            }
            reads[b].extend(at.keys().copied());
            for header in loops.headers(b).filter(|&header| header != b) {
                reads[header].extend(at.keys().copied());
            }
            first.push(at);
        }
        for (block, reads) in blocks.iter_mut().zip(reads) {
            block.reads = reads;
        }
        let livein: Vec<HashSet<u32>> = blocks
            .iter()
            .map(|block| block.livein.iter().copied().collect())
            .collect();
        let mut changed = true;
        while changed {
            changed = false;
            for &b in order.iter().rev() {
                let exits = |succ: usize| loops.header(b).is_some_and(|h| !loops.contains(h, succ));
                let mut next_out = std::mem::take(&mut blocks[b].next_out);
                for &value in &blocks[b].liveout {
                    let distance = blocks[b]
                        .succs
                        .iter()
                        .filter(|&&succ| livein[succ].contains(&value))
                        .filter_map(|&succ| {
                            let inside = first[succ].get(&value).copied().or_else(|| {
                                let past = blocks[succ].next_out.get(&value)?;
                                Some(blocks[succ].ops.len() as u64 + past)
                            })?;
                            Some(inside + if exits(succ) { LOOP_EXIT } else { 0 })
                        })
                        .min();
                    if let Some(distance) = distance
                        && next_out.get(&value).is_none_or(|&known| distance < known)
                    {
                        next_out.insert(value, distance);
                        changed = true;
                    }
                }
                blocks[b].next_out = next_out;
            }
        }
    }

    fn choose_memory_phis(&mut self) {
        // After Hack: a parameter the block reads (a loop header: its loop
        // reads) keeps a register while the file holds it; one it only passes
        // through keeps one only where the values it reads leave room at every
        // op (of the loop, Braun & Hack § 4.2); the rest stay in memory.
        let Self {
            capacity,
            shapes,
            blocks,
            pinned,
            remat,
            loops,
            memory,
            dead,
        } = self;
        let files = capacity.len();
        let (own, demand) = demands(blocks, loops, files);
        for (b, block) in blocks.iter_mut().enumerate() {
            block.room = capacity.clone();
            if b == 0 || block.preds.is_empty() {
                continue;
            }
            let mut first: HashMap<u32, u64> = HashMap::new();
            for (index, op) in block.ops.iter().enumerate().rev() {
                for value in op.timed(remat) {
                    first.insert(value, index as u64);
                }
            }
            let next = |value: &u32| match first.get(value) {
                Some(&index) => index,
                None => block.past(*value),
            };
            let passive = |value: &u32| !block.reads.contains(value) && next(value) != DEAD;
            let mut through = vec![0; files];
            let mut peak = vec![0; files];
            for value in block.params.iter().chain(&block.livein) {
                let Some(shape) = shapes.get(value) else {
                    continue;
                };
                if passive(value) {
                    through[shape.file] += shape.width;
                } else if block.reads.contains(value) {
                    peak[shape.file] += shape.width;
                }
            }
            // A loop header's parameter the header itself does not read must
            // also outlast the header's own ops to keep its register.
            let unread = |value: &u32| !first.contains_key(value) && next(value) != DEAD;
            let mut passing = vec![0; files];
            let mut reading = vec![0; files];
            for value in block.params.iter().chain(&block.livein) {
                if let Some(shape) = shapes.get(value) {
                    if unread(value) {
                        passing[shape.file] += shape.width;
                    } else if first.contains_key(value) {
                        reading[shape.file] += shape.width;
                    }
                }
            }
            let mut local: Vec<u32> = (0..files)
                .map(|file| {
                    let peak = reading[file].max(own[b][file].saturating_sub(passing[file]));
                    capacity[file].saturating_sub(peak)
                })
                .collect();
            for file in 0..files {
                peak[file] = peak[file].max(demand[b][file].saturating_sub(through[file]));
            }
            let mut params = block.params.clone();
            params.sort_by_key(|value| (!pinned.contains(value), next(value), *value));
            // Should some parameter stay in memory, its edge may reload the
            // argument while the others are in registers: leave it one.
            let mut wanted = vec![0; files];
            let mut widest = vec![0; files];
            for shape in params
                .iter()
                .filter(|param| next(param) != DEAD)
                .filter_map(|param| shapes.get(param))
            {
                wanted[shape.file] += shape.width;
                widest[shape.file] = widest[shape.file].max(shape.width);
            }
            let held: Vec<u32> = (0..files)
                .map(|file| match wanted[file] > capacity[file] {
                    true => capacity[file] - widest[file],
                    false => capacity[file],
                })
                .collect();
            let mut taken = vec![0; files];
            let mut room: Vec<u32> = (0..files)
                .map(|file| capacity[file].saturating_sub(peak[file]))
                .collect();
            for param in params {
                let Some(shape) = shapes.get(&param) else {
                    continue;
                };
                let file = shape.file;
                let kept = if pinned.contains(&param) {
                    true
                } else if next(&param) == DEAD {
                    false
                } else if passive(&param) {
                    let fits = shape.width <= room[file];
                    if fits {
                        room[file] -= shape.width;
                    }
                    fits
                } else if unread(&param) {
                    let fits =
                        taken[file] + shape.width <= held[file] && shape.width <= local[file];
                    if fits {
                        local[file] -= shape.width;
                    }
                    fits
                } else {
                    taken[file] + shape.width <= held[file]
                };
                if kept {
                    taken[file] += shape.width;
                } else {
                    memory.insert(param);
                    if next(&param) == DEAD {
                        dead.insert(param);
                    }
                }
            }
            block.room = room;
        }
    }

    // What `op` needs in registers: its operands, but for the arguments
    // it passes only to memory parameters.
    fn needs(&self, op: &Op) -> SmallVec<[u32; 4]> {
        let in_memory = |value: &u32| {
            let mut passed = op.args.iter().filter(|(_, arg, _)| arg == value);
            passed.clone().next().is_some()
                && passed.all(|(param, _, _)| self.memory.contains(param))
        };
        op.uses
            .iter()
            .copied()
            .filter(|value| !in_memory(value))
            .collect()
    }

    fn fits(&self) -> bool {
        // A block's entry and each op's write point, where the op's results
        // join what stays live and a clobber takes its registers from what
        // lives across it.
        let files = self.capacity.len();
        for block in &self.blocks {
            let mut entry = vec![0; files];
            for value in block.params.iter().chain(&block.livein) {
                if let Some(shape) = self.shapes.get(value) {
                    entry[shape.file] += shape.width;
                }
            }
            if entry
                .iter()
                .zip(&self.capacity)
                .any(|(load, cap)| load > cap)
            {
                return false;
            }
            for (op, fixed) in block.ops.iter().zip(fixed(&block.ops, files)) {
                for file in 0..files {
                    let cap = self.capacity[file];
                    if op.written[file] + fixed.after[file] > cap
                        || op.live[file] + fixed.across[file] > cap
                        || op.through[file] + fixed.at[file] > cap
                    {
                        return false;
                    }
                }
            }
        }
        true
    }
}

// Per block, the most units its ops take at once, and the same over the loop
// a header heads.
fn demands(blocks: &[Block], loops: &Loops, files: usize) -> (Vec<Vec<u32>>, Vec<Vec<u32>>) {
    let own: Vec<Vec<u32>> = blocks
        .iter()
        .map(|block| {
            let mut peak = vec![0; files];
            for (op, fixed) in block.ops.iter().zip(fixed(&block.ops, files)) {
                for (file, peak) in peak.iter_mut().enumerate() {
                    let written = op.written[file] + fixed.after[file];
                    let at = op.through[file] + fixed.at[file];
                    *peak = (*peak)
                        .max(written)
                        .max(at)
                        .max(op.live[file] + fixed.across[file]);
                }
            }
            peak
        })
        .collect();
    let mut demand = own.clone();
    for (b, inner) in own.iter().enumerate() {
        for header in loops.headers(b).filter(|&header| header != b) {
            for (peak, &inner) in demand[header].iter_mut().zip(inner) {
                *peak = (*peak).max(inner);
            }
        }
    }
    (own, demand)
}

fn read_edges(context: &Context, blocks: &[BlockId], db: &RegAllocDb, out: &mut [Block]) {
    let index_of: HashMap<BlockId, usize> = blocks
        .iter()
        .enumerate()
        .map(|(index, &block)| (block, index))
        .collect();
    for (pred, &block) in blocks.iter().enumerate() {
        let mut successors: Vec<usize> = Vec::new();
        for (index, op_id) in context.get_block(block).op_ids().into_iter().enumerate() {
            let op = context.get_op(op_id);
            let through_vbr = op.is::<VirtualBranchOp>();
            if through_vbr && let Some(AttributeValue::Block(dest)) = op.attr("dest") {
                let params = context.get_block(dest).value_arguments();
                for (position, (param, &arg)) in
                    params.iter().zip(op.value_operands().iter()).enumerate()
                {
                    if let (Some(param), Some(arg)) = (db.value_id(param.id()), db.value_id(arg)) {
                        out[pred].ops[index].args.push((param, arg, position));
                    }
                }
            }
            let Some(term) = op.as_interface::<dyn Terminator>() else {
                continue;
            };
            for succ in term.successors() {
                let Some(&succ) = index_of.get(&succ) else {
                    continue;
                };
                if !successors.contains(&succ) {
                    successors.push(succ);
                    out[succ].preds.push(pred);
                }
                out[succ].phis &= through_vbr;
            }
        }
        out[pred].succs = successors;
    }
}

fn windows(ops: &[Op]) -> Vec<usize> {
    // Per op, the last op of the window it lies in. A register an op names
    // directly holds its value from the op that writes it to the op that
    // reads it; a reload placed between the two could be given that register,
    // so the reloads of a window go before its first op.
    let mut last: Vec<usize> = (0..ops.len()).collect();
    for (index, op) in ops.iter().enumerate() {
        let mut pending = op.fixed_uses.clone();
        let mut start = index;
        while !pending.is_empty() && start > 0 {
            start -= 1;
            pending.retain(|unit| !ops[start].fixed_defs.contains(unit));
            pending.extend(ops[start].fixed_uses.iter().copied());
        }
        last[start] = last[start].max(index);
    }
    // Overlapping windows are one.
    let mut reach = vec![0; ops.len()];
    let mut begin = 0;
    while begin < ops.len() {
        let (mut end, mut next) = (last[begin], begin);
        while next <= end {
            end = end.max(last[next]);
            next += 1;
        }
        reach[begin..next].fill(end);
        begin = next;
    }
    reach
}

// What a register holds a value under: an SSA value, or the parameter a
// block may take for it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Name {
    Value(ValueId),
    Phi(usize),
}

// A value a block holds on entry under whatever name its predecessors pass:
// a parameter where they differ.
struct Phi {
    block: usize,
    value: u32,
    // Per predecessor, in the block's order, what it holds the value under.
    incoming: Vec<Name>,
    alias: Option<Name>,
    param: Option<ValueId>,
}

enum Edit {
    Store {
        slot: Slot,
        name: Name,
    },
    Reload {
        slot: Slot,
        fresh: ValueId,
        replay: Option<OpId>,
    },
}

// A frame slot: a value's own, or the second one of its class an edge breaks
// a cycle of memory parameters through.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Slot {
    Of(u32),
    Aside(u32),
}

impl Slot {
    fn value(self) -> u32 {
        match self {
            Slot::Of(value) | Slot::Aside(value) => value,
        }
    }
}

// What an edge stores into the slot of a memory parameter.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Transfer {
    Name(Name),
    Slot(Slot),
}

// The registers at one point: the values they hold and, per file, the units
// those take.
struct Registers {
    held: Vec<(u32, Name)>,
    load: Vec<u32>,
}

impl Registers {
    fn name(&self, value: u32) -> Option<Name> {
        self.held
            .iter()
            .find(|(held, _)| *held == value)
            .map(|&(_, name)| name)
    }

    fn insert(&mut self, shape: Shape, value: u32, name: Name) {
        self.held.push((value, name));
        self.load[shape.file] += shape.width;
    }

    fn retain(&mut self, shapes: &HashMap<u32, Shape>, keep: impl Fn(u32) -> bool) {
        let load = &mut self.load;
        self.held.retain(|&(value, _)| {
            let kept = keep(value);
            if !kept {
                let shape = shapes[&value];
                load[shape.file] -= shape.width;
            }
            kept
        });
    }
}

struct Spiller<'a> {
    function: &'a Function,
    db: &'a RegAllocDb,
    context: &'a Context,
    // Per visited block, the registers it ends with.
    end: Vec<Option<Vec<(u32, Name)>>>,
    entry_phis: Vec<Vec<usize>>,
    phis: Vec<Phi>,
    // Operands and results to rename: `(op, index, name, original)`.
    operands: Vec<(OpId, usize, Name, ValueId)>,
    results: Vec<(OpId, usize, Name, ValueId)>,
    // Per block, `(index, phase, edit)`: what goes before the op at
    // `index`, in phase order.
    edits: Vec<Vec<(usize, u8, Edit)>>,
    // The `asm.vbr` operands passing a memory parameter.
    removed: Vec<(OpId, usize)>,
    // The values reloaded from a slot, in the order first reloaded.
    stored: Vec<u32>,
    replayed: Vec<u32>,
    // Per value, where its store goes, `(block, before op index)`, and the
    // name its last definition writes.
    defs: HashMap<u32, (usize, usize, Name)>,
}

impl<'a> Spiller<'a> {
    fn new(function: &'a Function, db: &'a RegAllocDb, context: &'a Context) -> Self {
        let blocks = function.blocks.len();
        Self {
            function,
            db,
            context,
            end: (0..blocks).map(|_| None).collect(),
            entry_phis: vec![Vec::new(); blocks],
            phis: Vec::new(),
            operands: Vec::new(),
            results: Vec::new(),
            edits: (0..blocks).map(|_| Vec::new()).collect(),
            removed: Vec::new(),
            stored: Vec::new(),
            replayed: Vec::new(),
            defs: HashMap::new(),
        }
    }

    fn block(&mut self, b: usize) -> Result<(), PassError> {
        let function = self.function;
        let block = &function.blocks[b];
        let mut reads: HashMap<u32, Vec<usize>> = HashMap::new();
        for (index, op) in block.ops.iter().enumerate() {
            for value in op.timed(&function.remat) {
                reads.entry(value).or_default().push(index);
            }
        }
        let next = |value: u32, from: usize| -> u64 {
            let at = reads.get(&value).and_then(|reads| {
                let first = reads.partition_point(|&index| index < from);
                reads.get(first)
            });
            match at {
                Some(&index) => index as u64,
                None => block.past(value),
            }
        };
        let mut registers = self.entry(b, &next)?;
        let files = function.capacity.len();
        let reach = windows(&block.ops);
        let fixed = fixed(&block.ops, files);
        let left = |taken: &[u32]| -> Vec<u32> {
            let capacity = function.capacity.iter();
            capacity
                .zip(taken)
                .map(|(cap, taken)| cap.saturating_sub(*taken))
                .collect()
        };
        for (index, op) in block.ops.iter().enumerate() {
            let (before, after_op, across, at) = (
                left(&fixed[index].before),
                left(&fixed[index].after),
                left(&fixed[index].across),
                left(&fixed[index].at),
            );
            let handle = self.context.get_op(op.id);
            let needs = function.needs(op);
            let window = &block.ops[index..=reach[index]];
            // What the window reads past this op, unless the window defines it
            // first. A window that reads more than the file holds reloads at
            // each read, as if it were none.
            let mut later: SmallVec<[u32; 8]> = (1..window.len())
                .flat_map(|at| {
                    let defined =
                        move |value: &u32| window[..at].iter().any(|op| op.defs.contains(value));
                    function
                        .needs(&window[at])
                        .into_iter()
                        .filter(move |value| !defined(value))
                })
                .collect();
            let mut load = registers.load.clone();
            for value in needs.iter().chain(&later) {
                if let Some(shape) = function.shapes.get(value)
                    && registers.name(*value).is_none()
                {
                    load[shape.file] += shape.width;
                }
            }
            if load.iter().zip(&before).any(|(load, cap)| load > cap) {
                later.clear();
            }
            // The slots of memory parameters this edge fills: the edge reads
            // what they hold before any of them is written.
            let filled: SmallVec<[u32; 4]> = op
                .args
                .iter()
                .filter(|&&(param, arg, _)| {
                    function.memory.contains(&param)
                        && arg != param
                        && !function.dead.contains(&param)
                })
                .map(|&(param, _, _)| param)
                .collect();
            let stored = |value: u32| {
                filled.contains(&value)
                    || op
                        .args
                        .iter()
                        .any(|&(param, arg, _)| arg == value && filled.contains(&param))
            };
            for &value in needs.iter().chain(&later) {
                let Some(&shape) = function.shapes.get(&value) else {
                    continue;
                };
                if registers.name(value).is_none() {
                    let phase = if stored(value) { EDGE_LOAD } else { RELOAD };
                    let name = self.reload(b, index, value, phase)?;
                    registers.insert(shape, value, name);
                }
            }
            for &value in &needs {
                let Some(name) = registers.name(value) else {
                    continue;
                };
                let original = self.db.value_of(value);
                for (position, _) in handle
                    .operands()
                    .iter()
                    .enumerate()
                    .filter(|(_, operand)| **operand == original)
                {
                    self.operands.push((op.id, position, name, original));
                }
            }
            // A memory parameter's slot is filled on the edge: from the
            // register its argument is in, or through one register at a time.
            let mut copies = vec![0; files];
            let mut transfers: Vec<(u32, Transfer)> = Vec::new();
            for &(param, arg, position) in &op.args {
                if !function.memory.contains(&param) {
                    continue;
                }
                self.removed.push((op.id, position));
                if arg == param || function.dead.contains(&param) {
                    continue;
                }
                let from = match registers.name(arg) {
                    Some(name) => Transfer::Name(name),
                    None => {
                        if let Some(shape) = function.shapes.get(&arg) {
                            copies[shape.file] = copies[shape.file].max(shape.width);
                        }
                        Transfer::Slot(Slot::Of(arg))
                    }
                };
                transfers.push((param, from));
            }
            self.fill(b, index, transfers)?;
            let after = |value| next(value, index + 1);
            self.fit(
                &mut registers,
                &before,
                &copies,
                |value| needs.contains(&value) || later.contains(&value),
                after,
            )?;

            // A read-modify-write keeps its register across the op.
            let rewritten: SmallVec<[(u32, Name); 1]> = op
                .defs
                .iter()
                .filter_map(|&def| Some((def, registers.name(def)?)))
                .collect();
            registers.retain(&function.shapes, |value| {
                after(value) != DEAD && !op.defs.contains(&value)
            });
            let mut written = vec![0; files];
            let mut live = vec![0; files];
            for def in &op.defs {
                if let Some(shape) = function.shapes.get(def) {
                    written[shape.file] += shape.width;
                    if after(*def) != DEAD {
                        live[shape.file] += shape.width;
                    }
                }
            }
            let kept = |value| later.contains(&value);
            self.fit(&mut registers, &after_op, &written, kept, after)?;
            self.fit(&mut registers, &across, &live, kept, after)?;
            self.fit(&mut registers, &at, &vec![0; files], kept, after)?;

            for &def in &op.defs {
                let original = self.db.value_of(def);
                let name = rewritten
                    .iter()
                    .find(|(value, _)| *value == def)
                    .map_or(Name::Value(original), |&(_, name)| name);
                if name != Name::Value(original) {
                    for (position, _) in handle
                        .results()
                        .iter()
                        .enumerate()
                        .filter(|(_, result)| **result == original)
                    {
                        self.results.push((op.id, position, name, original));
                    }
                }
                self.defs.insert(def, (b, index + 1, name));
                if let Some(&shape) = function.shapes.get(&def)
                    && after(def) != DEAD
                {
                    registers.insert(shape, def, name);
                }
            }
        }
        // A reload for the closing branch runs on its edge alone; the other
        // edges leave before it.
        if block.succs.len() > 1
            && let Some(last) = block.ops.len().checked_sub(1)
        {
            let edge_only: Vec<Name> = self.edits[b]
                .iter()
                .filter_map(|(index, _, edit)| match edit {
                    Edit::Reload { fresh, .. } if *index == last => Some(Name::Value(*fresh)),
                    _ => None,
                })
                .collect();
            registers.held.retain(|(_, name)| !edge_only.contains(name));
        }
        self.end[b] = Some(registers.held);
        Ok(())
    }

    fn entry(
        &mut self,
        b: usize,
        next: &impl Fn(u32, usize) -> u64,
    ) -> Result<Registers, PassError> {
        let function = self.function;
        let block = &function.blocks[b];
        let mut registers = Registers {
            held: Vec::new(),
            load: vec![0; function.capacity.len()],
        };
        for &param in &block.params {
            if function.memory.contains(&param) {
                continue;
            }
            let name = Name::Value(self.db.value_of(param));
            if let Some(&shape) = function.shapes.get(&param) {
                registers.insert(shape, param, name);
            }
            self.defs.insert(param, (b, 0, name));
        }
        if registers
            .load
            .iter()
            .zip(&function.capacity)
            .any(|(load, cap)| load > cap)
        {
            return Err(PassError::InvalidRuleSet(format!(
                "block {b} takes more parameters than its register files hold"
            )));
        }

        let mut candidates = self.candidates(b);
        let pinned = |value: &u32| function.pinned.contains(value);
        candidates.sort_by_key(|&(value, _, everywhere)| {
            (!pinned(&value), !everywhere, next(value, 0), value)
        });
        let joins = block.preds.len() > 1 && block.phis;
        let header = function.loops.is_header(b);
        let mut room = block.room.clone();
        for (value, name, _) in candidates {
            let shape = function.shapes[&value];
            let passive = header && !block.reads.contains(&value) && !pinned(&value);
            if registers.load[shape.file] + shape.width > function.capacity[shape.file]
                || passive && room[shape.file] < shape.width
            {
                if pinned(&value) {
                    return Err(PassError::InvalidRuleSet(format!(
                        "pinned %{} does not fit on entry to block {b}",
                        self.db.value_of(value).number()
                    )));
                }
                continue;
            }
            let name = if joins {
                self.phis.push(Phi {
                    block: b,
                    value,
                    incoming: Vec::new(),
                    alias: None,
                    param: None,
                });
                self.entry_phis[b].push(self.phis.len() - 1);
                Name::Phi(self.phis.len() - 1)
            } else {
                name
            };
            if passive {
                room[shape.file] -= shape.width;
            }
            registers.insert(shape, value, name);
        }
        registers.retain(&function.shapes, |value| next(value, 0) != DEAD);
        Ok(registers)
    }

    fn candidates(&self, b: usize) -> Vec<(u32, Name, bool)> {
        // Braun & Hack § 4.2: a loop header may take any value live into it,
        // reloaded on the edge from a predecessor that does not hold it;
        // another block takes what its predecessors end with, first what all
        // of them hold. Each candidate says whether every predecessor visited
        // holds it. A predecessor not visited yet is a back edge: it can
        // reload on the edge only through an `asm.vbr` it ends with alone. A
        // conditional branch carries no arguments, so a block it enters takes
        // only what every predecessor holds under one name.
        let function = self.function;
        let block = &function.blocks[b];
        let ends: Vec<&Vec<(u32, Name)>> = block
            .preds
            .iter()
            .filter_map(|&pred| self.end[pred].as_ref())
            .collect();
        let everywhere = |value: u32| {
            ends.iter()
                .all(|end| end.iter().any(|&(held, _)| held == value))
        };
        let mut candidates: Vec<(u32, Name, bool)> = Vec::new();
        if function.loops.is_header(b) && block.phis {
            for &value in &block.livein {
                let name = Name::Value(self.db.value_of(value));
                candidates.push((value, name, everywhere(value)));
            }
        } else {
            for &(value, name) in ends.iter().copied().flatten() {
                if block.livein.contains(&value)
                    && !candidates.iter().any(|&(held, _, _)| held == value)
                {
                    candidates.push((value, name, everywhere(value)));
                }
            }
        }
        if block.preds.len() == 1 {
            return candidates;
        }
        for &pred in &block.preds {
            let Some(end) = &self.end[pred] else {
                if !block.phis || function.blocks[pred].succs.len() > 1 {
                    return Vec::new();
                }
                continue;
            };
            if !block.phis {
                candidates.retain(|&(value, name, _)| end.contains(&(value, name)));
            } else if function.blocks[pred].succs.len() > 1 {
                candidates.retain(|(value, _, _)| end.iter().any(|(held, _)| held == value));
            }
        }
        candidates
    }

    fn fit(
        &self,
        registers: &mut Registers,
        limit: &[u32],
        extra: &[u32],
        keep: impl Fn(u32) -> bool,
        next: impl Fn(u32) -> u64,
    ) -> Result<(), PassError> {
        let function = self.function;
        for file in 0..limit.len() {
            while registers.load[file] + extra[file] > limit[file] {
                let farthest = registers
                    .held
                    .iter()
                    .map(|&(value, _)| value)
                    .filter(|&value| {
                        function.shapes[&value].file == file
                            && !keep(value)
                            && !function.pinned.contains(&value)
                    })
                    .max_by_key(|&value| (next(value), function.remat.contains_key(&value), value))
                    .ok_or_else(|| {
                        PassError::InvalidRuleSet(
                            "register pressure exceeds the file at one instruction".to_string(),
                        )
                    })?;
                registers.retain(&function.shapes, |value| value != farthest);
            }
        }
        Ok(())
    }

    fn reload(&mut self, b: usize, index: usize, value: u32, phase: u8) -> Result<Name, PassError> {
        let function = self.function;
        let original = self.db.value_of(value);
        if function.pinned.contains(&value) {
            return Err(PassError::InvalidRuleSet(format!(
                "pinned %{} would be reloaded",
                original.number()
            )));
        }
        let (fresh, replay) = match function.remat.get(&value) {
            Some(&def) => {
                if !self.replayed.contains(&value) {
                    self.replayed.push(value);
                }
                let (replay, fresh) =
                    RegisterAllocationPass::rematerialize(self.context, def, original)?;
                (fresh, Some(replay))
            }
            None => {
                if !function.memory.contains(&value) && !self.stored.contains(&value) {
                    self.stored.push(value);
                }
                (fresh_reg(self.context, function.shapes[&value].class), None)
            }
        };
        self.edits[b].push((
            index,
            phase,
            Edit::Reload {
                slot: Slot::Of(value),
                fresh,
                replay,
            },
        ));
        Ok(Name::Value(fresh))
    }

    // Fill the slots of memory parameters an edge passes as one parallel
    // copy: a slot is stored to once nothing left reads it, and a cycle is
    // broken through a second slot.
    fn fill(
        &mut self,
        b: usize,
        index: usize,
        mut transfers: Vec<(u32, Transfer)>,
    ) -> Result<(), PassError> {
        let read = |transfers: &[(u32, Transfer)], param: u32| {
            transfers
                .iter()
                .any(|&(_, from)| from == Transfer::Slot(Slot::Of(param)))
        };
        // Where no slot is read after it is filled, the stores from registers
        // go first.
        let ordered = transfers.iter().any(|&(param, _)| read(&transfers, param));
        let named = if ordered { COPY } else { EDGE_STORE };
        while !transfers.is_empty() {
            let Some(at) = transfers
                .iter()
                .position(|&(param, _)| !read(&transfers, param))
            else {
                let (param, _) = transfers[0];
                let name = self.reload(b, index, param, COPY)?;
                let aside = Slot::Aside(param);
                self.edits[b].push((index, COPY, Edit::Store { slot: aside, name }));
                for (_, from) in &mut transfers {
                    if *from == Transfer::Slot(Slot::Of(param)) {
                        *from = Transfer::Slot(aside);
                    }
                }
                continue;
            };
            let (param, from) = transfers.remove(at);
            let (phase, name) = match from {
                Transfer::Name(name) => (named, name),
                Transfer::Slot(Slot::Of(value)) => (COPY, self.reload(b, index, value, COPY)?),
                Transfer::Slot(slot) => {
                    let class = self.function.shapes[&slot.value()].class;
                    let fresh = fresh_reg(self.context, class);
                    let replay = None;
                    let reload = Edit::Reload {
                        slot,
                        fresh,
                        replay,
                    };
                    self.edits[b].push((index, COPY, reload));
                    (COPY, Name::Value(fresh))
                }
            };
            let store = Edit::Store {
                slot: Slot::Of(param),
                name,
            };
            self.edits[b].push((index, phase, store));
        }
        Ok(())
    }

    fn join(&mut self, blocks: &[BlockId]) -> Result<(), PassError> {
        let function = self.function;
        self.drop_unread_phis();
        for b in 0..function.blocks.len() {
            for &pred in &function.blocks[b].preds {
                for phi in self.entry_phis[b].clone() {
                    let value = self.phis[phi].value;
                    let held = self.end[pred]
                        .as_ref()
                        .and_then(|end| end.iter().find(|(held, _)| *held == value))
                        .map(|&(_, name)| name);
                    let name = match held {
                        Some(name) => name,
                        None => {
                            let branch = self.branch(pred, b, blocks)?;
                            self.reload(pred, branch, value, RELOAD)?
                        }
                    };
                    self.phis[phi].incoming.push(name);
                }
            }
        }

        let mut changed = true;
        while changed {
            changed = false;
            for phi in 0..self.phis.len() {
                if self.phis[phi].alias.is_some() {
                    continue;
                }
                let mut only = None;
                let mut trivial = true;
                for &incoming in &self.phis[phi].incoming {
                    let incoming = self.alias(incoming);
                    if incoming == Name::Phi(phi) || only == Some(incoming) {
                        continue;
                    }
                    if only.is_some() {
                        trivial = false;
                        break;
                    }
                    only = Some(incoming);
                }
                if trivial && let Some(only) = only {
                    self.phis[phi].alias = Some(only);
                    changed = true;
                }
            }
        }

        for &index in self.entry_phis.iter().flatten() {
            let phi = &mut self.phis[index];
            if phi.alias.is_none() {
                let original = self.db.value_of(phi.value);
                let ty = self.context.get_value(original).ty();
                phi.param = Some(
                    self.context
                        .append_block_argument(blocks[phi.block], ty)
                        .id(),
                );
            }
        }
        for b in 0..function.blocks.len() {
            let real: Vec<usize> = self.entry_phis[b]
                .iter()
                .copied()
                .filter(|&phi| self.phis[phi].param.is_some())
                .collect();
            for (edge, &pred) in function.blocks[b].preds.iter().enumerate() {
                if real.is_empty() {
                    break;
                }
                let branch = function.blocks[pred].ops[self.branch(pred, b, blocks)?].id;
                for &phi in &real {
                    let arg = self.resolve(self.phis[phi].incoming[edge]);
                    let op = self.context.get_op(branch);
                    let (at, end) = (op.value_operands().len(), op.operands().len());
                    self.context.insert_operand_at(branch, at, arg, end);
                }
            }
        }
        Ok(())
    }

    // Keep only the phis something reads: an operand, a result, a store, or a
    // phi kept on an edge it is held on. A value a block took on entry but
    // evicted before any read needs no reload on the edges into it.
    fn drop_unread_phis(&mut self) {
        let function = self.function;
        let mut needed: Vec<usize> = self
            .operands
            .iter()
            .chain(&self.results)
            .map(|&(_, _, name, _)| name)
            .chain(
                self.edits
                    .iter()
                    .flatten()
                    .filter_map(|(_, _, edit)| match edit {
                        Edit::Store { name, .. } => Some(*name),
                        Edit::Reload { .. } => None,
                    }),
            )
            .filter_map(|name| match name {
                Name::Phi(phi) => Some(phi),
                Name::Value(_) => None,
            })
            .collect();
        let mut kept = vec![false; self.phis.len()];
        while let Some(phi) = needed.pop() {
            if std::mem::replace(&mut kept[phi], true) {
                continue;
            }
            let Phi { block, value, .. } = self.phis[phi];
            for &pred in &function.blocks[block].preds {
                let held = self.end[pred]
                    .as_ref()
                    .and_then(|end| end.iter().find(|(held, _)| *held == value));
                if let Some(&(_, Name::Phi(incoming))) = held {
                    needed.push(incoming);
                }
            }
        }
        for phis in &mut self.entry_phis {
            phis.retain(|&phi| kept[phi]);
        }
    }

    fn branch(&self, pred: usize, b: usize, blocks: &[BlockId]) -> Result<usize, PassError> {
        self.function.blocks[pred]
            .ops
            .iter()
            .rposition(|op| {
                let op = self.context.get_op(op.id);
                op.is::<VirtualBranchOp>()
                    && matches!(op.attr("dest"), Some(AttributeValue::Block(dest)) if dest == blocks[b])
            })
            .ok_or_else(|| {
                PassError::InvalidRuleSet(format!("block {pred} has no asm.vbr to block {b}"))
            })
    }

    fn alias(&self, mut name: Name) -> Name {
        while let Name::Phi(phi) = name
            && let Some(alias) = self.phis[phi].alias
        {
            name = alias;
        }
        name
    }

    fn resolve(&self, name: Name) -> ValueId {
        match self.alias(name) {
            Name::Value(value) => value,
            Name::Phi(phi) => self.phis[phi]
                .param
                .expect("a phi left standing has a parameter"),
        }
    }

    fn emit(
        mut self,
        pass: &RegisterAllocationPass,
        blocks: &[BlockId],
        frame: &mut FramePlan,
    ) -> Result<(), PassError> {
        let context = self.context;
        let function = self.function;
        for &value in &self.stored {
            let (block, index, name) = self.defs[&value];
            let slot = Slot::Of(value);
            self.edits[block].push((index, DEF_STORE, Edit::Store { slot, name }));
        }
        let frame_register = pass.frame_register();
        let mut slots: HashMap<Slot, i64> = HashMap::new();
        for (b, mut edits) in std::mem::take(&mut self.edits).into_iter().enumerate() {
            edits.sort_by_key(|&(index, phase, _)| (index, phase));
            let mut chains: HashMap<Slot, SlotChain> = HashMap::new();
            for (index, _, edit) in edits.iter() {
                let before = op_ref_in(context, function.blocks[b].ops[*index].id);
                match *edit {
                    Edit::Reload {
                        replay: Some(replay),
                        ..
                    } => {
                        let replay = context.get_op(replay).as_dyn_op();
                        context.insert_op_before(&before, replay.as_ref())?;
                    }
                    Edit::Reload { slot, fresh, .. } => {
                        let class = function.shapes[&slot.value()].class;
                        let offset = *slots.entry(slot).or_insert_with(|| {
                            frame.alloc(
                                pass.target.spill_slot_size(class, pass.abi),
                                pass.abi.stack.slot_size,
                            )
                        });
                        let reload = pass.target.emit_spill_reload(
                            context,
                            fresh,
                            class,
                            &frame_register,
                            offset,
                        );
                        let chain = chains.entry(slot).or_default();
                        chain.observe(context, &before, reload.as_ref())?;
                        context.insert_op_before(&before, reload.as_ref())?;
                    }
                    Edit::Store { slot, name } => {
                        let class = function.shapes[&slot.value()].class;
                        let offset = *slots.entry(slot).or_insert_with(|| {
                            frame.alloc(
                                pass.target.spill_slot_size(class, pass.abi),
                                pass.abi.stack.slot_size,
                            )
                        });
                        let name = self.resolve(name);
                        let store = pass.target.emit_spill_store(
                            context,
                            name,
                            class,
                            &frame_register,
                            offset,
                        );
                        let chain = chains.entry(slot).or_default();
                        chain.change(context, &before, store.as_ref())?;
                        context.insert_op_before(&before, store.as_ref())?;
                    }
                }
            }
        }
        for &(op, position, name, original) in &self.operands {
            let renamed = self.resolve(name);
            if renamed != original {
                context.set_op_operand(op, position, renamed);
            }
        }
        for &(op, position, name, original) in &self.results {
            let renamed = self.resolve(name);
            if renamed != original {
                context.set_op_result(op, position, renamed);
            }
        }
        self.removed
            .sort_by_key(|&(_, position)| std::cmp::Reverse(position));
        for &(op, position) in &self.removed {
            context.remove_operand(op, position);
        }
        for (b, block) in function.blocks.iter().enumerate() {
            for param in block
                .params
                .iter()
                .filter(|param| function.memory.contains(param))
            {
                let param = self.db.value_of(*param);
                let arguments = context.get_block(blocks[b]).arguments();
                if let Some(index) = arguments.iter().position(|argument| argument.id() == param) {
                    context.remove_block_argument(blocks[b], index);
                }
            }
        }
        for value in self.replayed {
            let def = function.remat[&value];
            if context.has_operation(def) && !context.is_used(self.db.value_of(value)) {
                context.erase_op(&op_ref_in(context, def))?;
            }
        }
        Ok(())
    }
}
