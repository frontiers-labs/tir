//! An operation's run: the one span holding its operands, results and region
//! ids, plus the use-list links that thread every operand entry onto the value
//! it names.
//!
//! Runs are spans of one arena rather than a heap block per op, so an op costs
//! no allocation of its own and an erased op's span is handed to the next op of
//! that size. The layout inside a run is operands, then results, then regions;
//! the counts live on the op.
//!
//! Spans are sized in powers of two, so a run that grows one port at a time
//! moves logarithmically often rather than on every push.

use crate::OpId;

/// No entry: the end of a use list, or a value nothing reads.
pub(crate) const NO_ENTRY: u32 = u32::MAX;

/// The largest size class. Class 15 names [`Span::NONE`], so a span holds at
/// most `1 << 14` items.
const MAX_CLASS: u32 = 14;
const CLASSES: usize = MAX_CLASS as usize + 1;
/// Items one span may hold.
const MAX_LEN: usize = 1 << MAX_CLASS;
/// Items one arena may hold; the rest of a [`Span`] is its size class.
const MAX_ITEMS: usize = 1 << 28;

/// A run of items in an arena: a size class and the index of its first item.
///
/// The empty run has no span at all: [`Span::NONE`] names no storage.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Span(u32);

impl Span {
    const NONE: Span = Span(u32::MAX);

    fn new(class: u32, start: u32) -> Self {
        Span(class << 28 | start)
    }

    fn class(self) -> u32 {
        self.0 >> 28
    }

    fn start(self) -> usize {
        (self.0 & (MAX_ITEMS as u32 - 1)) as usize
    }

    fn capacity(self) -> usize {
        if self == Span::NONE {
            0
        } else {
            1 << self.class()
        }
    }

    fn range(self) -> std::ops::Range<usize> {
        if self == Span::NONE {
            return 0..0;
        }
        self.start()..self.start() + self.capacity()
    }
}

/// The smallest class holding `len` items.
fn class_for(len: usize) -> u32 {
    len.next_power_of_two().trailing_zeros()
}

/// A pool of same-typed items handed out in power-of-two spans.
///
/// A freed span is not reused until [`Arena::recycle`] says the caller is done
/// with the ids it handed out, so an id held across a free cannot start
/// answering for a stranger mid-pass.
struct Arena<T> {
    items: Vec<T>,
    /// Spans free for reuse, by size class.
    free: [Vec<u32>; CLASSES],
    /// Spans freed since the last [`Arena::recycle`].
    pending: Vec<Span>,
    live: usize,
}

impl<T> Default for Arena<T> {
    fn default() -> Self {
        Arena {
            items: Vec::new(),
            free: Default::default(),
            pending: Vec::new(),
            live: 0,
        }
    }
}

impl<T: Clone> Arena<T> {
    /// A span of `len` items, every one of them `blank`.
    fn alloc(&mut self, len: usize, blank: T) -> Span {
        if len == 0 {
            return Span::NONE;
        }
        assert!(len <= MAX_LEN, "a run holds at most {MAX_LEN} items");
        let class = class_for(len);
        self.live += 1;
        match self.free[class as usize].pop() {
            Some(start) => {
                let span = Span::new(class, start);
                self.slice_mut(span).fill(blank);
                span
            }
            None => {
                let start = self.items.len();
                assert!(start + (1 << class) <= MAX_ITEMS, "arena exhausted");
                self.items.resize(start + (1 << class), blank);
                Span::new(class, start as u32)
            }
        }
    }

    fn free(&mut self, span: Span) {
        if span == Span::NONE {
            return;
        }
        self.live -= 1;
        self.pending.push(span);
    }

    fn recycle(&mut self) {
        for span in self.pending.drain(..) {
            self.free[span.class() as usize].push(span.start() as u32);
        }
    }

    fn slice(&self, span: Span) -> &[T] {
        &self.items[span.range()]
    }

    fn slice_mut(&mut self, span: Span) -> &mut [T] {
        &mut self.items[span.range()]
    }

    /// Live spans and the bytes the arena holds, for the memory census.
    fn census(&self) -> (usize, usize) {
        (self.live, self.items.capacity() * size_of::<T>())
    }
}

/// One position in a run: the id it holds, its place in the use list of the
/// value that id names when it is an operand, and the op whose run it is part
/// of.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Entry {
    pub id: u32,
    pub next: u32,
    pub prev: u32,
    /// Who owns the slot, so a use list walked from a value answers which
    /// operation it arrived at.
    pub owner: OpId,
}

impl Entry {
    const fn new(id: u32, owner: OpId) -> Self {
        Entry {
            id,
            next: NO_ENTRY,
            prev: NO_ENTRY,
            owner,
        }
    }

    /// Point the slot at `id`, in no use list. The owner belongs to the slot,
    /// not to the id, so it stays.
    pub(crate) fn reset(&mut self, id: u32) {
        self.id = id;
        self.next = NO_ENTRY;
        self.prev = NO_ENTRY;
    }
}

/// The span an op's entries live in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct RunId(Span);

impl RunId {
    pub(crate) const NONE: RunId = RunId(Span::NONE);

    /// Entries the run holds, live or spare.
    pub(crate) fn capacity(self) -> usize {
        self.0.capacity()
    }

    /// The arena index of the run's first entry.
    pub(crate) fn start(self) -> usize {
        self.0.start()
    }

    /// The address of the entry at `offset`.
    pub(crate) fn entry(self, offset: usize) -> EntryId {
        debug_assert!(offset < self.capacity());
        EntryId((self.0.start() + offset) as u32)
    }
}

/// The address of one entry: its index in the run arena.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct EntryId(u32);

impl EntryId {
    pub(crate) fn from_raw(raw: u32) -> Self {
        EntryId(raw)
    }

    pub(crate) fn raw(self) -> u32 {
        self.0
    }
}

#[derive(Default)]
pub(crate) struct Runs(Arena<Entry>);

impl Runs {
    /// A run of `len` empty entries owned by `owner`.
    fn reserve(&mut self, owner: OpId, len: usize) -> RunId {
        RunId(self.0.alloc(len, Entry::new(0, owner)))
    }

    pub(crate) fn free(&mut self, run: RunId) {
        self.0.free(run.0);
    }

    pub(crate) fn entries(&self, run: RunId) -> &[Entry] {
        self.0.slice(run.0)
    }

    pub(crate) fn entries_mut(&mut self, run: RunId) -> &mut [Entry] {
        self.0.slice_mut(run.0)
    }

    pub(crate) fn entry(&self, id: EntryId) -> &Entry {
        &self.0.items[id.0 as usize]
    }

    pub(crate) fn entry_mut(&mut self, id: EntryId) -> &mut Entry {
        &mut self.0.items[id.0 as usize]
    }

    /// Move `run`'s first `live` entries into a run holding `needed`, freeing
    /// the old span. Entry addresses change, so the caller relinks.
    pub(crate) fn grow(&mut self, owner: OpId, run: RunId, live: usize, needed: usize) -> RunId {
        debug_assert!(live <= needed);
        let ids: Vec<u32> = self.entries(run)[..live].iter().map(|e| e.id).collect();
        let grown = self.reserve(owner, needed);
        for (entry, id) in self.entries_mut(grown).iter_mut().zip(&ids) {
            entry.id = *id;
        }
        self.free(run);
        grown
    }

    pub(crate) fn recycle(&mut self) {
        self.0.recycle();
    }

    /// Live runs and the bytes the arena holds, for the memory census.
    pub(crate) fn census(&self) -> (usize, usize) {
        self.0.census()
    }
}

type Attr = crate::attributes::NamedAttribute;

/// A filler for a run's spare capacity: the attribute count on the op bounds
/// what is ever read, so this is never observed.
fn filler() -> Attr {
    Attr::new(
        tir_adt::Sym::default(),
        crate::attributes::AttributeValue::Bool(false),
    )
}

/// The span an op's attributes live in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct AttrRunId(Span);

impl AttrRunId {
    pub(crate) const NONE: AttrRunId = AttrRunId(Span::NONE);
}

/// Attributes get the same treatment as ports: a span of one arena, so reading
/// one is a single index and an op with attributes costs no heap block of its
/// own.
#[derive(Default)]
pub(crate) struct AttrRuns(Arena<Attr>);

impl AttrRuns {
    /// Store `attributes` as one run. An empty list gets no span at all.
    pub(crate) fn alloc(&mut self, attributes: Vec<Attr>) -> AttrRunId {
        let run = AttrRunId(self.0.alloc(attributes.len(), filler()));
        for (slot, attribute) in self.0.slice_mut(run.0).iter_mut().zip(attributes) {
            *slot = attribute;
        }
        run
    }

    /// Release the run, dropping the attributes it holds: a payload of an
    /// erased op must not stay alive until the span is handed out again.
    pub(crate) fn free(&mut self, run: AttrRunId) {
        self.0.slice_mut(run.0).fill_with(filler);
        self.0.free(run.0);
    }

    pub(crate) fn get(&self, run: AttrRunId, count: usize) -> &[Attr] {
        &self.0.slice(run.0)[..count]
    }

    pub(crate) fn get_mut(&mut self, run: AttrRunId, count: usize) -> &mut [Attr] {
        &mut self.0.slice_mut(run.0)[..count]
    }

    pub(crate) fn recycle(&mut self) {
        self.0.recycle();
    }

    /// Live runs and the bytes the arena holds, for the memory census.
    pub(crate) fn census(&self) -> (usize, usize) {
        self.0.census()
    }
}
