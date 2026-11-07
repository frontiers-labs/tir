//! Rule heads: what a match writes. A head only adds — a row, an equality —
//! so a match that fired cannot be un-fired, and a head reads nothing beyond
//! the key lookup an insert is. A match exists in epoch `t` and not `t-1`
//! exactly when one of its rows or values is from epoch `t-1`, which is what
//! lets a semi-naive driver search only the delta.
//!
//! A head runs on the matches of its rule, write by write inside one batch:
//! the keys an insert comes to are looked up as columns, and a later write
//! reads what an earlier one bound for the same match. A match's equates are
//! part of the partition later matches insert against, so a batch stops before
//! a match whose key would change once they land — otherwise that match mints
//! a node an earlier equate already made the same as one the table holds.

use smallvec::SmallVec;

use crate::Ref;
use crate::db::{Database, LatticeId, SpaceId};
use crate::query::{Expr, Matches, Plan, Scalar, UNBOUND, Var, bound};
use crate::store::Values;

/// One write of a rule's head. Variables hold references.
#[derive(Clone, Debug)]
pub enum Head {
    /// Bind `into` to the value of the row the client keys by `args`. If no
    /// row has the key, the client inserts one with a fresh value id.
    Insert {
        args: SmallVec<[Var; 4]>,
        /// Whether the arguments are sorted, once canonical, before the
        /// client keys them.
        sorted: bool,
        into: Var,
    },
    Equate(Var, Var),
    /// Equate `var` with variable `offset + scalars[index]`: the one head that
    /// chooses among bound variables, for a rule that picks an argument by a
    /// guard's answer.
    EquateIndexed {
        var: Var,
        offset: Var,
        index: Scalar,
    },
    /// Bind `into` to `base` plus what `offset` evaluates to.
    Offset {
        base: Var,
        offset: Expr,
        into: Var,
    },
    /// Join what `value` evaluates to onto what `key`'s id is known to be in
    /// `column`.
    Raise {
        column: LatticeId,
        key: Var,
        value: Expr,
    },
}

/// The key of a row: its cells, and beside them the weights of the cells
/// whose offsets stay in the key.
#[derive(Default)]
pub struct Key {
    pub cells: SmallVec<[u32; 8]>,
    pub weights: SmallVec<[u64; 8]>,
    /// Whether some weight is non-zero.
    pub weighted: bool,
}

impl Key {
    pub fn clear(&mut self) {
        self.cells.clear();
        self.weights.clear();
        self.weighted = false;
    }

    /// The weights as a table takes them: none when every one is zero.
    pub fn weights(&self) -> &[u64] {
        match self.weighted {
            true => &self.weights,
            false => &[],
        }
    }
}

/// What an insert comes to before its table is probed.
pub enum Keyed {
    /// The client settles it without a row.
    Is(Ref),
    /// The key is built: the table its row lives in, and what the row's value
    /// is shifted by.
    At { table: u32, shift: u64 },
}

/// A rule as a driver runs it: what to look for. What it proves, its head,
/// the driver asks the client for at the rule's first match.
#[derive(Clone, Copy)]
pub struct Rule<'a> {
    pub name: &'a str,
    pub plan: &'a Plan,
    /// Variables the head binds, numbered above the query's.
    pub head_vars: u32,
    /// Applied once after the fixpoint rather than in it, for a rule whose
    /// head would feed back through the others.
    pub post_saturation: bool,
}

/// What a head asks its client: the database, and the parts of a write only
/// the client can do.
pub trait Client<C, const S: usize> {
    fn db(&self) -> &Database<C, S>;
    fn db_mut(&mut self) -> &mut Database<C, S>;
    /// What insert `op` of rule `rule` over `operands`, which are canonical,
    /// comes to given the match's scalars: a reference, or the key its row is
    /// stored under, written into `key`. `None`: the client cannot spell it,
    /// and the head stops.
    fn key(
        &mut self,
        rule: usize,
        op: usize,
        operands: &[Ref],
        scalars: &[u64],
        key: &mut Key,
    ) -> Option<Keyed>;
    /// The tag of the row the next [`Self::mint`] is for. Each mint takes
    /// the next tag.
    fn tag(&self) -> u32;
    /// Mint the value id of the row just stored under `key` in `table`, plus
    /// whatever the client keeps per row, returning the id: the next one of
    /// the space.
    fn mint(&mut self, rule: usize, op: usize, table: u32, key: &Key, scalars: &[u64]) -> u32;
    /// Equate two references, plus whatever the client keeps per id.
    fn equate(&mut self, a: Ref, b: Ref);
}

/// What a driver keeps between the rules it applies: per match of the rule in
/// hand its variables, and the inserts being looked up together.
#[derive(Default)]
pub struct Batch {
    // Per match, its bindings and then the head's own variables.
    held: Vec<Ref>,
    // Per match, whether its head stopped, and whether it wrote anything.
    stopped: Vec<bool>,
    wrote: Vec<bool>,
    run: Run,
    key: Key,
    // Ids a match in the open batch equated, stamped with `stamp_epoch`.
    stamps: Vec<u32>,
    stamp_epoch: u32,
    // Matches of earlier batches of this apply that wrote nothing.
    carried_idle: usize,
}

impl Batch {
    /// How many matches of the last rule applied wrote nothing: equated
    /// nothing, minted nothing and raised nothing.
    pub fn idle(&self) -> usize {
        self.carried_idle + self.wrote.iter().filter(|&&wrote| !wrote).count()
    }
}

// Consecutive inserts of one head write into one table: their key columns,
// and per key the match it is of and the offset its row's value is at from
// the insert's.
#[derive(Default)]
struct Run {
    table: u32,
    arity: usize,
    columns: Vec<Vec<u32>>,
    weights: Vec<Vec<u64>>,
    weighted: bool,
    matches: Vec<u32>,
    shifts: Vec<u64>,
    found: Vec<Option<Ref>>,
}

impl Run {
    fn push(&mut self, table: u32, key: &Key, matched: u32, shift: u64) {
        let arity = key.cells.len();
        if self.matches.is_empty() {
            (self.table, self.arity) = (table, arity);
            if self.columns.len() < arity {
                self.columns.resize_with(arity, Vec::new);
                self.weights.resize_with(arity, Vec::new);
            }
        }
        if key.weighted && !self.weighted {
            self.weighted = true;
            for weights in &mut self.weights[..arity] {
                weights.resize(self.matches.len(), 0);
            }
        }
        for (column, &cell) in self.columns.iter_mut().zip(&key.cells) {
            column.push(cell);
        }
        if self.weighted {
            for (at, column) in self.weights[..arity].iter_mut().enumerate() {
                column.push(key.weights.get(at).copied().unwrap_or(0));
            }
        }
        self.matches.push(matched);
        self.shifts.push(shift);
    }

    fn clear(&mut self) {
        for column in &mut self.columns[..self.arity] {
            column.clear();
        }
        for weights in &mut self.weights[..self.arity] {
            weights.clear();
        }
        self.weighted = false;
        self.matches.clear();
        self.shifts.clear();
    }
}

/// Run `head` on the matches of rule `rule`, whose variables range over
/// `space`, one write after another: each write on every match, in match
/// order, before the next. A missing binding or an insert the client cannot
/// spell stops the rest of that match's head; everything before it stands,
/// which is sound because a head only adds.
///
/// An equate is visible to the writes after it through the partition alone:
/// the tables name the absorbed ids until the client canonicalizes them.
/// It is also visible to later matches. Matches that would insert a different
/// key once an earlier equate is in the partition run in a later batch, so a
/// column lookup still covers every match whose key does not depend on one.
pub fn apply<C, const S: usize>(
    client: &mut impl Client<C, S>,
    space: SpaceId,
    rule: usize,
    head: &[Head],
    head_vars: u32,
    matches: &Matches,
    batch: &mut Batch,
) {
    batch.carried_idle = 0;
    let count = matches.len();
    if count <= 1 || !head_merges_and_inserts(head) {
        apply_batch(client, space, rule, head, head_vars, matches, batch);
        return;
    }
    let calls = (rule, head, head_vars);
    let mut start = 0;
    let mut epoch = fresh_epoch(batch, client.db().ids(space));
    let mut marked = 0usize;
    for index in 0..count {
        if marked > 0
            && reads_merged(
                client.db(),
                space,
                calls.1,
                matches,
                index,
                &batch.stamps,
                epoch,
            )
        {
            apply_range(client, space, calls, matches, start..index, batch);
            batch.carried_idle = batch.idle();
            start = index;
            epoch = fresh_epoch(batch, client.db().ids(space));
            marked = 0;
        }
        marked += mark_merged(
            client.db(),
            space,
            calls.1,
            matches,
            index,
            &mut batch.stamps,
            epoch,
        );
    }
    apply_range(client, space, calls, matches, start..count, batch);
}

fn head_merges_and_inserts(head: &[Head]) -> bool {
    let mut inserts = false;
    let mut equates = false;
    for write in head {
        match write {
            Head::Insert { .. } => inserts = true,
            Head::Equate(..) | Head::EquateIndexed { .. } => equates = true,
            Head::Offset { .. } | Head::Raise { .. } => {}
        }
    }
    inserts && equates
}

fn fresh_epoch(batch: &mut Batch, ids: usize) -> u32 {
    if batch.stamps.len() < ids {
        batch.stamps.resize(ids, 0);
    }
    let next = batch.stamp_epoch.wrapping_add(1);
    if next == 0 {
        batch.stamps.fill(0);
        batch.stamp_epoch = 1;
        1
    } else {
        batch.stamp_epoch = next;
        next
    }
}

fn apply_range<C, const S: usize>(
    client: &mut impl Client<C, S>,
    space: SpaceId,
    (rule, head, head_vars): (usize, &[Head], u32),
    matches: &Matches,
    range: std::ops::Range<usize>,
    batch: &mut Batch,
) {
    if range.start == 0 && range.end == matches.len() {
        apply_batch(client, space, rule, head, head_vars, matches, batch);
        return;
    }
    let mut slice = Matches::default();
    for index in range {
        let (root, bindings, scalars) = matches.get(index);
        slice.push(root, bindings, scalars);
    }
    apply_batch(client, space, rule, head, head_vars, &slice, batch);
}

// The class `var` names: a binding the match arrived with, or the base of the
// offset a head var was bound to. A var the head inserts is a new id, and no
// later match can hold it.
fn source_class<C, const S: usize>(
    db: &Database<C, S>,
    space: SpaceId,
    head: &[Head],
    bindings: &[Ref],
    mut var: Var,
) -> Option<u32> {
    for _ in 0..head.len() {
        if (var as usize) < bindings.len()
            && let Some(reference) = bound(bindings[var as usize])
        {
            return Some(db.find(space, reference).id);
        }
        let base = head.iter().find_map(|write| match write {
            Head::Offset { base, into, .. } if *into == var => Some(*base),
            _ => None,
        })?;
        var = base;
    }
    None
}

fn mark_merged<C, const S: usize>(
    db: &Database<C, S>,
    space: SpaceId,
    head: &[Head],
    matches: &Matches,
    index: usize,
    stamps: &mut Vec<u32>,
    epoch: u32,
) -> usize {
    let (_, bindings, scalars) = matches.get(index);
    let mut marked = 0;
    for write in head {
        let vars = match write {
            Head::Equate(a, b) => [*a, *b],
            Head::EquateIndexed { var, offset, index } => {
                [*var, *offset + scalars[*index as usize] as Var]
            }
            Head::Insert { .. } | Head::Offset { .. } | Head::Raise { .. } => continue,
        };
        for var in vars {
            let Some(id) = source_class(db, space, head, bindings, var) else {
                continue;
            };
            let at = id as usize;
            if stamps.len() <= at {
                stamps.resize(at + 1, 0);
            }
            if stamps[at] != epoch {
                stamps[at] = epoch;
                marked += 1;
            }
        }
    }
    marked
}

fn reads_merged<C, const S: usize>(
    db: &Database<C, S>,
    space: SpaceId,
    head: &[Head],
    matches: &Matches,
    index: usize,
    stamps: &[u32],
    epoch: u32,
) -> bool {
    let (_, bindings, _) = matches.get(index);
    for write in head {
        let Head::Insert { args, .. } = write else {
            continue;
        };
        for &arg in args {
            let Some(id) = source_class(db, space, head, bindings, arg) else {
                continue;
            };
            if stamps.get(id as usize).copied() == Some(epoch) {
                return true;
            }
        }
    }
    false
}

fn apply_batch<C, const S: usize>(
    client: &mut impl Client<C, S>,
    space: SpaceId,
    rule: usize,
    head: &[Head],
    head_vars: u32,
    matches: &Matches,
    batch: &mut Batch,
) {
    let count = matches.len();
    batch.held.clear();
    for index in 0..count {
        let (_, bindings, _) = matches.get(index);
        batch.held.extend_from_slice(bindings);
        let own = batch.held.len() + head_vars as usize;
        batch.held.resize(own, UNBOUND);
    }
    let width = batch.held.len().checked_div(count).unwrap_or(0);
    batch.stopped.clear();
    batch.stopped.resize(count, false);
    batch.wrote.clear();
    batch.wrote.resize(count, false);
    let get = |held: &[Ref], index: usize, var: Var| {
        bound(*held[index * width..][..width].get(var as usize)?)
    };
    for (op, write) in head.iter().enumerate() {
        match write {
            Head::Insert { args, sorted, into } => {
                let insert = Insert {
                    at: (rule, op),
                    args,
                    sorted: *sorted,
                    into: *into as usize,
                    width,
                };
                batch_insert(client, space, &insert, matches, batch);
            }
            Head::Equate(..) | Head::EquateIndexed { .. } => {
                for index in 0..count {
                    let (a, b) = match write {
                        Head::EquateIndexed {
                            var,
                            offset,
                            index: scalar,
                        } => {
                            let scalars = matches.get(index).2;
                            (*var, *offset + scalars[*scalar as usize] as Var)
                        }
                        Head::Equate(a, b) => (*a, *b),
                        _ => unreachable!("an equate"),
                    };
                    let bound = (get(&batch.held, index, a), get(&batch.held, index, b));
                    let (false, (Some(a), Some(b))) = (batch.stopped[index], bound) else {
                        batch.stopped[index] = true;
                        continue;
                    };
                    let before = work(client);
                    client.equate(a, b);
                    batch.wrote[index] |= work(client) != before;
                }
            }
            Head::Offset { base, offset, into } => {
                for index in 0..count {
                    let scalars = matches.get(index).2;
                    let bound = (get(&batch.held, index, *base), offset.eval(scalars));
                    let (false, (Some(base), Some(offset))) = (batch.stopped[index], bound) else {
                        batch.stopped[index] = true;
                        continue;
                    };
                    let shifted = Ref::new(base.id, base.offset.wrapping_add(offset as u64));
                    batch.held[index * width + *into as usize] = client.db().find(space, shifted);
                }
            }
            Head::Raise { column, key, value } => {
                for index in 0..count {
                    let scalars = matches.get(index).2;
                    let bound = (get(&batch.held, index, *key), value.eval(scalars));
                    let (false, (Some(key), Some(value))) = (batch.stopped[index], bound) else {
                        batch.stopped[index] = true;
                        continue;
                    };
                    let key = client.db().find(space, key).id;
                    batch.wrote[index] |= client.db_mut().raise(*column, key, value as u64);
                }
            }
        }
    }
}

// What a write that changed anything moved.
fn work<C, const S: usize>(client: &impl Client<C, S>) -> (usize, usize, usize) {
    let stats = client.db().stats();
    (stats.merges, stats.adds, stats.raises)
}

struct Insert<'a> {
    at: (usize, usize),
    args: &'a [Var],
    sorted: bool,
    into: usize,
    // How many variables a match holds.
    width: usize,
}

// Binds each match's `into` to the value of the row the client keys by its
// `args`. The keys of consecutive matches that fall into one table are
// looked up together, in match order: of equal keys the first mints and the
// rest find it.
fn batch_insert<C, const S: usize>(
    client: &mut impl Client<C, S>,
    space: SpaceId,
    insert: &Insert<'_>,
    matches: &Matches,
    batch: &mut Batch,
) {
    let (rule, op) = insert.at;
    let mut operands: SmallVec<[Ref; 4]> = SmallVec::new();
    for index in 0..matches.len() {
        if batch.stopped[index] {
            continue;
        }
        let held = &mut batch.held[index * insert.width..][..insert.width];
        operands.clear();
        let db = client.db();
        operands.extend(
            insert
                .args
                .iter()
                .map_while(|&arg| Some(db.find(space, bound(*held.get(arg as usize)?)?))),
        );
        if operands.len() < insert.args.len() {
            batch.stopped[index] = true;
            continue;
        }
        if insert.sorted {
            operands.sort_unstable();
        }
        batch.key.clear();
        let scalars = matches.get(index).2;
        let before = work(client);
        let keyed = client.key(rule, op, &operands, scalars, &mut batch.key);
        batch.wrote[index] |= work(client) != before;
        match keyed {
            None => batch.stopped[index] = true,
            Some(Keyed::Is(found)) => held[insert.into] = found,
            Some(Keyed::At { table, shift }) => {
                if batch.run.table != table {
                    store(client, space, insert, matches, batch);
                }
                batch.run.push(table, &batch.key, index as u32, shift);
            }
        }
    }
    store(client, space, insert, matches, batch);
}

// Look the run's keys up and mint the missing ones.
fn store<C, const S: usize>(
    client: &mut impl Client<C, S>,
    space: SpaceId,
    insert: &Insert<'_>,
    matches: &Matches,
    batch: &mut Batch,
) {
    let run = &mut batch.run;
    if run.matches.is_empty() {
        return;
    }
    let (rule, op) = insert.at;
    let arity = run.arity;
    let keys: SmallVec<[&[u32]; 8]> = run.columns[..arity].iter().map(Vec::as_slice).collect();
    let weights: SmallVec<[&[u64]; 8]> = match run.weighted {
        true => run.weights[..arity].iter().map(Vec::as_slice).collect(),
        false => SmallVec::new(),
    };
    let first = client.db().ids(space) as u32;
    let tag = client.tag();
    client.db_mut().insert_rows(
        run.table,
        &keys,
        &weights,
        Values::From(first),
        tag,
        &mut run.found,
    );
    let key = &mut batch.key;
    let mut minted = first;
    for (at, &index) in run.matches.iter().enumerate() {
        let shift = run.shifts[at];
        let value = match run.found[at] {
            Some(found) => {
                let found = Ref::new(found.id, found.offset.wrapping_add(shift));
                client.db().find(space, found)
            }
            None => {
                key.clear();
                key.cells.extend(keys.iter().map(|column| column[at]));
                key.weights.resize(arity, 0);
                for (weight, column) in key.weights.iter_mut().zip(&weights) {
                    *weight = column[at];
                    key.weighted |= *weight != 0;
                }
                let scalars = matches.get(index as usize).2;
                batch.wrote[index as usize] = true;
                let id = client.mint(rule, op, run.table, key, scalars);
                debug_assert_eq!(id, minted, "the value the row was stored with");
                minted += 1;
                Ref::new(id, shift)
            }
        };
        batch.held[index as usize * insert.width + insert.into] = value;
    }
    drop((keys, weights));
    run.clear();
}
