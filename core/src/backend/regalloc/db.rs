//! The engine database register allocation reads a function through: one
//! [`Database`] per function, one id space shared by its blocks, operations,
//! values and rows.
//!
//! Every table keeps the constant `0` in its first, plain column: a row atom
//! selects its table by that cell. Tables that carry a scalar in their key
//! (a register, a weight) keep it in the second plain column.

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use smallvec::SmallVec;
use tir::{BlockId, Context, OpId, Terminator, ValueId};
use tir_relational::query::{Atom, Calls, Host, Resolution};
use tir_relational::rule::{Client, Head, Key, Keyed};
use tir_relational::saturate::{Caches, Delta, Saturate, saturate};
use tir_relational::store::Table;
use tir_relational::{Database, Join, LatticeId, Ref, SpaceId};

use super::live::Rule;
use crate::backend::liveness::{PhysReg, op_regs};
use crate::backend::{RegSlot, VirtualBranchOp};

pub(super) const SPACE: SpaceId = SpaceId::nth(0);

// Seeded tables, keyed `(key cells) -> value`.
// `[op] -> block`, tag = the op's use point (four per op, see `point`).
pub(super) const HOLDS: u32 = 0;
// `[op, value] -> row`: the op reads the value.
pub(super) const USES: u32 = 1;
// `[op, value] -> row`: the op defines the value.
pub(super) const DEFS: u32 = 2;
// `[block, value] -> row`: a definition or parameter of the block.
pub(super) const DEFINED_IN: u32 = 3;
// `[block, value] -> row`: a block parameter, defined on entry.
pub(super) const PARAM: u32 = 4;
// `[pred, succ] -> row`.
pub(super) const EDGE: u32 = 5;
// `[pred, param] -> argument`, tag = the edge's weight: the value arguments
// of an `asm.vbr`. State arguments are not phis.
pub(super) const PHI: u32 = 6;
// `[source, destination] -> row`, tag = the copy's weight.
pub(super) const COPY: u32 = 7;
// `[source, destination] -> row`: a copy whose destination nothing else
// defines, so both ends hold one value wherever both are live.
pub(super) const SAME: u32 = 8;
// `[op] -> op` bag, tag = a register the op clobbers.
pub(super) const CLOBBER: u32 = 9;
// `[value] -> row`, tag = the register the value is pinned to.
pub(super) const PIN: u32 = 10;
// `[weight, value] -> row`, tag = a register a copy moves the value to or
// from.
pub(super) const PREFER: u32 = 11;
// Derived tables.
// `[block, value] -> row`.
pub(super) const LIVEIN: u32 = 12;
// `[block, value] -> row`.
pub(super) const LIVEOUT: u32 = 13;
// `[block, value] -> range`; the range keys `START` and `END`.
pub(super) const RANGE: u32 = 14;
// `[op, value] -> row`: the value is in a register after the op.
pub(super) const LIVE_AT: u32 = 15;
// `[a, b] -> row`, an unordered pair.
pub(super) const INTERFERE: u32 = 16;
// `[register, value] -> row`: the value is live across a clobber of it.
pub(super) const FORBID: u32 = 17;
// `[weight, a, b] -> row`.
pub(super) const AFFINITY: u32 = 18;

// The schema, in slot order: `(plain columns, id columns, bag)`.
const SCHEMA: [(usize, usize, bool); 19] = [
    (1, 1, false), // HOLDS
    (1, 2, false), // USES
    (1, 2, false), // DEFS
    (1, 2, false), // DEFINED_IN
    (1, 2, false), // PARAM
    (1, 2, false), // EDGE
    (1, 2, true),  // PHI
    (1, 2, true),  // COPY
    (1, 2, false), // SAME
    (1, 1, true),  // CLOBBER
    (1, 1, false), // PIN
    (2, 1, true),  // PREFER
    (1, 2, false), // LIVEIN
    (1, 2, false), // LIVEOUT
    (1, 2, false), // RANGE
    (1, 2, false), // LIVE_AT
    (1, 2, false), // INTERFERE
    (2, 1, false), // FORBID
    (2, 2, false), // AFFINITY
];
pub(super) const TABLES: u32 = SCHEMA.len() as u32;

// The lattices a range is bounded by, in the order `schema` adds them.
pub(super) const START: LatticeId = LatticeId::nth(0);
pub(super) const END: LatticeId = LatticeId::nth(1);

// Where a block's range of a value is open past its last operation.
pub(super) const EXIT: i64 = 1 << 40;

// The four program points of the op at position `index` of its block (from
// zero): it reads at `point`, writes at `point + 1`, a value it defines and
// nobody reads ends at `point + 2`, and a register it clobbers is lost at
// `point + 2`. A block parameter is written at 1 and a value live on entry
// starts at 0.
pub(super) fn point(index: usize) -> u32 {
    4 * (index as u32 + 1)
}

// A parallel copy inserted right before the op at `index` reads and writes
// at one point, past everything of the op before it.
pub(super) fn before(index: usize) -> u32 {
    point(index) - 1
}

// The indices of a block's `len` ops after which a range `[start, end)`
// holds its value: the op at `i` does when `start <= point(i) + 1` and
// `point(i) + 2 < end`.
pub(super) fn held_after(start: i64, end: i64, len: usize) -> Range<usize> {
    let first = (start - 2).div_euclid(4).max(0) as usize;
    let last = (end - 3).div_euclid(4).clamp(0, len as i64) as usize;
    first.min(last)..last
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Named {
    Block(BlockId),
    Op(OpId),
    Value(ValueId),
    Row,
}

pub(super) struct SeededCopy {
    pub(super) block: usize,
    pub(super) src: RegSlot,
    pub(super) dst: RegSlot,
}

pub(super) struct Seed<'a> {
    pub(super) blocks: &'a [BlockId],
    // Per block, what one execution of it costs relative to the entry.
    pub(super) weights: &'a [u64],
    pub(super) copies: &'a [SeededCopy],
    pub(super) pins: &'a HashMap<ValueId, PhysReg>,
    pub(super) allocatable: &'a dyn Fn(PhysReg) -> bool,
}

pub(super) struct RegAllocDb {
    pub(super) db: Database<(), 1>,
    named: Vec<Named>,
    values: HashMap<ValueId, u32, tir_adt::FxBuildHasher>,
    pub(super) block_ids: Vec<u32>,
    block_index: HashMap<BlockId, usize>,
    pub(super) ops: Vec<Vec<OpId>>,
    pub(super) op_ids: Vec<Vec<u32>>,
    // Per block, its parameters and the values used before any definition.
    pub(super) params: Vec<Vec<u32>>,
    // The registers tags name, by index.
    pub(super) registers: Vec<PhysReg>,
}

impl RegAllocDb {
    pub(super) fn seed(context: &Context, seed: &Seed<'_>, spare: Option<Database<(), 1>>) -> Self {
        let db = match spare {
            Some(mut db) => {
                db.clear();
                db
            }
            None => schema(),
        };
        let mut this = Self {
            db,
            named: Vec::new(),
            values: HashMap::default(),
            block_ids: Vec::with_capacity(seed.blocks.len()),
            block_index: HashMap::new(),
            ops: Vec::with_capacity(seed.blocks.len()),
            op_ids: Vec::with_capacity(seed.blocks.len()),
            params: Vec::with_capacity(seed.blocks.len()),
            registers: Vec::new(),
        };
        this.block_index = seed
            .blocks
            .iter()
            .enumerate()
            .map(|(index, &block)| (block, index))
            .collect();
        for &block in seed.blocks {
            let id = this.mint(Named::Block(block));
            this.block_ids.push(id);
        }

        let weight = |block: usize| u32::try_from(seed.weights[block]).unwrap_or(u32::MAX);
        let mut defined: Vec<bool> = Vec::new();
        let mut used_first: Vec<u32> = Vec::new();
        let loaded = seed
            .blocks
            .first()
            .map_or_else(HashSet::new, |&entry| stack_arguments(context, entry));
        for (index, &block) in seed.blocks.iter().enumerate() {
            let block_id = this.block_ids[index];
            let mut params = Vec::new();
            let params_of = context.get_block(block).value_arguments();
            for param in params_of
                .iter()
                .filter(|param| !loaded.contains(&param.id()))
            {
                let value = this.value(param.id());
                this.mark(&mut defined, value);
                params.push(value);
                this.row(DEFINED_IN, &[block_id, value], 0);
                this.row(PARAM, &[block_id, value], 0);
            }
            this.params.push(params);

            let ops = context.get_block(block).op_ids();
            let mut op_ids = Vec::with_capacity(ops.len());
            for (position, &op_id) in ops.iter().enumerate() {
                let op = context.get_op(op_id);
                let id = this.mint(Named::Op(op_id));
                op_ids.push(id);
                let at = point(position);
                this.db.insert(HOLDS, &[0, id], &[], (block_id, 0), at);
                let regs = op_regs(&op);
                for &value in regs.uses.iter().filter(|v| context.has_value(**v)) {
                    let value = this.value(value);
                    if !defined.get(value as usize).copied().unwrap_or(false) {
                        used_first.push(value);
                    }
                    this.row(USES, &[id, value], 0);
                }
                for &value in regs.defs.iter().filter(|v| context.has_value(**v)) {
                    let value = this.value(value);
                    this.mark(&mut defined, value);
                    this.row(DEFS, &[id, value], 0);
                    this.row(DEFINED_IN, &[block_id, value], 0);
                }
                for &register in &regs.phys_defs {
                    if (seed.allocatable)(register) {
                        let register = this.register(register);
                        this.db.insert(CLOBBER, &[0, id], &[], (id, 0), register);
                    }
                }
                if let Some(term) = op.clone().as_interface::<dyn Terminator>() {
                    for succ in term.successors() {
                        if let Some(&succ) = this.block_index.get(&succ) {
                            let succ = this.block_ids[succ];
                            this.row(EDGE, &[block_id, succ], 0);
                        }
                    }
                }
                if op.is::<VirtualBranchOp>()
                    && let Some(tir::attributes::AttributeValue::Block(dest)) = op.attr("dest")
                {
                    let params = context.get_block(dest).value_arguments();
                    let args = op.value_operands();
                    for (param, &arg) in params.iter().zip(args.iter()) {
                        if param.id() == arg {
                            continue;
                        }
                        let (param, arg) = (this.value(param.id()), this.value(arg));
                        this.db
                            .insert(PHI, &[0, block_id, param], &[], (arg, 0), weight(index));
                    }
                }
            }
            this.ops.push(ops);
            this.op_ids.push(op_ids);
            this.reserve_fixed_windows(context, seed, index);
        }
        // A value read with no definition before it in its block, and none
        // anywhere, is defined on entry: an argument the convention placed.
        for value in used_first {
            if !defined.get(value as usize).copied().unwrap_or(false) {
                let entry = this.block_ids[0];
                this.mark(&mut defined, value);
                this.params[0].push(value);
                this.row(DEFINED_IN, &[entry, value], 0);
                this.row(PARAM, &[entry, value], 0);
            }
        }

        let mut defs: HashMap<u32, u32> = HashMap::new();
        for &def in this.db.table(DEFS).expect("schema").column(2) {
            *defs.entry(def).or_default() += 1;
        }
        for copy in seed.copies {
            let weight = weight(copy.block);
            match (copy.src, copy.dst) {
                (RegSlot::Value(src), RegSlot::Value(dst)) => {
                    let (src, dst) = (this.value(src), this.value(dst));
                    let id = this.mint(Named::Row);
                    this.db.insert(COPY, &[0, src, dst], &[], (id, 0), weight);
                    if defs.get(&dst) == Some(&1) {
                        this.row(SAME, &[src, dst], 0);
                    }
                }
                (RegSlot::Value(value), RegSlot::Phys(register))
                | (RegSlot::Phys(register), RegSlot::Value(value)) => {
                    let value = this.value(value);
                    let register = this.register(register);
                    let id = this.mint(Named::Row);
                    this.db
                        .insert(PREFER, &[0, weight, value], &[], (id, 0), register);
                }
                _ => {}
            }
        }
        for (&value, &register) in seed.pins {
            let value = this.value(value);
            let register = this.register(register);
            let id = this.mint(Named::Row);
            this.db.insert(PIN, &[0, value], &[], (id, 0), register);
        }
        this
    }

    pub(super) fn into_database(self) -> Database<(), 1> {
        self.db
    }

    fn mint(&mut self, named: Named) -> u32 {
        let id = self.db.mint(SPACE, 0, false);
        debug_assert_eq!(id as usize, self.named.len());
        self.named.push(named);
        id
    }

    fn mark(&self, defined: &mut Vec<bool>, value: u32) {
        if defined.len() <= value as usize {
            defined.resize(self.named.len().max(value as usize + 1), false);
        }
        defined[value as usize] = true;
    }

    // A row of `table` keyed by `ids`, with a value of its own, unless the
    // table holds the key.
    fn row(&mut self, table: u32, ids: &[u32], tag: u32) {
        let mut key: SmallVec<[u32; 4]> = SmallVec::new();
        key.push(0);
        key.extend_from_slice(ids);
        if self.db.lookup(table, &key, &[]).is_none() {
            let id = self.mint(Named::Row);
            self.db.insert(table, &key, &[], (id, 0), tag);
        }
    }

    pub(super) fn value(&mut self, value: ValueId) -> u32 {
        if let Some(&id) = self.values.get(&value) {
            return id;
        }
        let id = self.mint(Named::Value(value));
        self.values.insert(value, id);
        id
    }

    pub(super) fn block_id(&self, block: BlockId) -> Option<u32> {
        self.block_index
            .get(&block)
            .map(|&index| self.block_ids[index])
    }

    pub(super) fn value_id(&self, value: ValueId) -> Option<u32> {
        self.values.get(&value).copied()
    }

    // A register an instruction writes and a later one reads by name — x86
    // `cdq` then `idiv`, both on `edx` — holds that instruction's result until
    // the read. The spiller already counts the register occupied across the
    // span. A value defined inside it writes whichever register it is given,
    // so coloring has to refuse this one; otherwise the definition becomes the
    // last writer and a later reordering keeps the fixed read on top of it.
    fn reserve_fixed_windows(&mut self, context: &Context, seed: &Seed<'_>, block: usize) {
        struct Touch {
            uses: SmallVec<[PhysReg; 2]>,
            defs: SmallVec<[PhysReg; 2]>,
            values: SmallVec<[ValueId; 2]>,
        }
        let touches: Vec<Touch> = self.ops[block]
            .iter()
            .map(|&op| {
                let regs = op_regs(&context.get_op(op));
                Touch {
                    uses: regs
                        .phys_uses
                        .into_iter()
                        .filter(|&register| (seed.allocatable)(register))
                        .collect(),
                    defs: regs
                        .phys_defs
                        .into_iter()
                        .filter(|&register| (seed.allocatable)(register))
                        .collect(),
                    values: regs.defs.into_iter().collect(),
                }
            })
            .collect();
        let mut written: Vec<(usize, PhysReg)> = Vec::new();
        for (index, touch) in touches.iter().enumerate() {
            for &used in &touch.uses {
                let Some((from, held)) = written.iter().rev().find_map(|&(at, defined)| {
                    defined
                        .0
                        .overlaps(defined.1, used.0, used.1)
                        .then_some((at, defined))
                }) else {
                    continue;
                };
                for mid in &touches[from + 1..index] {
                    for &value in &mid.values {
                        let register = self.register(held);
                        let value = self.value(value);
                        self.row(FORBID, &[register, value], 0);
                    }
                }
            }
            for &defined in &touch.defs {
                written.retain(|&(_, previous)| {
                    !previous.0.overlaps(previous.1, defined.0, defined.1)
                });
                written.push((index, defined));
            }
        }
    }

    fn register(&mut self, register: PhysReg) -> u32 {
        match self.registers.iter().position(|&known| known == register) {
            Some(index) => index as u32,
            None => {
                self.registers.push(register);
                self.registers.len() as u32 - 1
            }
        }
    }

    pub(super) fn named(&self, id: u32) -> Named {
        self.named[id as usize]
    }

    pub(super) fn value_of(&self, id: u32) -> ValueId {
        match self.named(id) {
            Named::Value(value) => value,
            other => panic!("id {id} names {other:?}, not a value"),
        }
    }

    pub(super) fn table(&self, slot: u32) -> &Table {
        self.db.table(slot).expect("a table of the schema")
    }

    pub(super) fn saturate(&mut self, rules: &[Rule], caches: &mut Caches) {
        // A saturation drains the change log at its fixpoint, and a new rule
        // set has matched nothing yet.
        self.db.mark_all_changed(SPACE);
        let generic: Vec<tir_relational::rule::Rule<'_>> = rules
            .iter()
            .map(|rule| tir_relational::rule::Rule {
                name: rule.name,
                plan: &rule.plan,
                head_vars: rule.head_vars,
                post_saturation: false,
            })
            .collect();
        // A set none of whose rules reads what another derives is at its
        // fixpoint after the round that searched everything.
        let rounds = if stratified(rules) { 1 } else { usize::MAX };
        let mut run = Run { db: self, rules };
        saturate(&mut run, caches, SPACE, &generic, rounds, usize::MAX);
    }
}

// Whether no rule of `rules` reads a table another writes or a value
// another raises.
fn stratified(rules: &[Rule]) -> bool {
    let written = |atom: &Atom| {
        let Atom::Rows(rows) = atom else {
            return false;
        };
        let writes = |rule: &Rule| {
            rule.inserts
                .iter()
                .flatten()
                .any(|insert| insert.table == rows.select.param)
        };
        rules.iter().any(writes)
    };
    rules.iter().all(|rule| {
        let query = rule.plan.query();
        let nots = query.nots.iter().flat_map(|not| &not.atoms);
        rule.head
            .iter()
            .all(|head| !matches!(head, Head::Raise { .. }))
            && !query.atoms.iter().chain(nots).any(written)
    })
}

// An empty database of the schema, its lattices START and END in that order.
fn schema() -> Database<(), 1> {
    let mut db: Database<(), 1> = Database::new();
    for &(plain, ids, bag) in &SCHEMA {
        let arity = plain + ids;
        let table = if bag {
            Table::bag(arity)
        } else {
            Table::new(arity)
        };
        db.add_table(table.plain(plain), SPACE);
    }
    db.add_lattice(SPACE, Join::Min);
    db.add_lattice(SPACE, Join::Max);
    db
}

struct Run<'a> {
    db: &'a mut RegAllocDb,
    rules: &'a [Rule],
}

impl Client<(), 1> for Run<'_> {
    fn db(&self) -> &Database<(), 1> {
        &self.db.db
    }

    fn db_mut(&mut self) -> &mut Database<(), 1> {
        &mut self.db.db
    }

    fn key(
        &mut self,
        rule: usize,
        op: usize,
        operands: &[Ref],
        scalars: &[u64],
        key: &mut Key,
    ) -> Option<Keyed> {
        let insert = self.rules[rule].inserts[op].as_ref()?;
        key.cells.push(0);
        key.cells.extend(
            insert
                .plain
                .iter()
                .map(|&slot| scalars[slot as usize] as u32),
        );
        key.cells.extend(operands.iter().map(|operand| operand.id));
        Some(Keyed::At {
            table: insert.table,
            shift: 0,
        })
    }

    fn tag(&self) -> u32 {
        0
    }

    fn mint(&mut self, _rule: usize, _op: usize, _table: u32, _key: &Key, _scalars: &[u64]) -> u32 {
        self.db.mint(Named::Row)
    }

    fn equate(&mut self, _a: Ref, _b: Ref) {
        unreachable!("no rule equates")
    }
}

// The entry block's parameters its ops define: a stack argument is defined
// by its load, not on entry.
fn stack_arguments(context: &Context, entry: BlockId) -> HashSet<ValueId> {
    let block = context.get_block(entry);
    let defined: HashSet<ValueId> = block
        .op_ids()
        .into_iter()
        .flat_map(|op| context.get_op(op).results().to_vec())
        .collect();
    block
        .value_arguments()
        .iter()
        .map(|param| param.id())
        .filter(|param| defined.contains(param))
        .collect()
}

// Every parameter names the table of its slot, read through the constant
// cell of the first column.
#[derive(Clone, Copy)]
pub(super) struct Schema;

impl Calls for Schema {
    fn call(&self, _id: u32, _args: &[u64], _out: &mut [u64]) -> bool {
        false
    }
}

impl Host for Schema {
    fn version(&self) -> u64 {
        1
    }

    fn resolve(&self, param: u32, since: Option<u64>, out: &mut Resolution) {
        if since.is_none() {
            out.tables.push((param, SmallVec::from_slice(&[0])));
        }
    }
}

impl Saturate<(), 1> for Run<'_> {
    type Frontier = ();

    fn version(&self) -> u64 {
        1
    }

    fn host(&self, _rule: usize) -> impl Host + '_ {
        Schema
    }

    type Shared<'a>
        = Schema
    where
        Self: 'a;

    fn shared(&self, _rule: usize) -> Option<Schema> {
        Some(Schema)
    }

    fn head(&self, rule: usize) -> Vec<Head> {
        self.rules[rule].head.clone()
    }

    fn roots(&mut self, rule: usize, frontier: Option<&mut Delta<()>>) -> Vec<u32> {
        match frontier {
            Some(delta) => {
                let height = self.rules[rule].plan.height();
                delta.at(&self.db.db, SPACE, height).to_vec()
            }
            None => (0..self.db.db.ids(SPACE) as u32).collect(),
        }
    }

    fn rebuild(&mut self) {}

    fn size(&self) -> usize {
        self.db.named.len()
    }
}
