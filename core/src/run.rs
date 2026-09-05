//! An operation's run: the one cell holding its operands, results and region
//! ids, plus the use-list links that thread every operand entry onto the value
//! it names.
//!
//! Runs live in size-classed [`Hive`]s rather than in a heap block per op, so
//! an op costs no allocation of its own and an erased op's cell is handed to
//! the next op of that size. The layout inside a run is operands, then results,
//! then regions; the counts live on the op.

use tir_adt::Hive;

use crate::OpId;

/// No entry: the end of a use list, or a value nothing reads.
///
/// `u32::MAX` decodes as the overflow class's last cell at offset 2047, so that
/// one address is reserved: [`Runs::alloc`] refuses a run of 2048 entries,
/// which keeps offset 2047 unreachable and this sentinel distinct from every
/// real entry.
pub(crate) const NO_ENTRY: u32 = u32::MAX;

/// Capacity of each fixed size class, by class number.
const CLASS_CAPACITY: [usize; 5] = [2, 4, 8, 16, 32];
/// The overflow class, for runs above the largest fixed capacity.
const BIG: u32 = 7;

/// One position in a run: the id it holds, and — for an operand entry — its
/// place in the use list of the value that id names.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Entry {
    pub id: u32,
    pub next: u32,
    pub prev: u32,
}

impl Entry {
    pub(crate) const fn new(id: u32) -> Self {
        Entry {
            id,
            next: NO_ENTRY,
            prev: NO_ENTRY,
        }
    }
}

/// The cell an op's entries live in: a size class and an index within it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct RunId(u32);

impl RunId {
    pub(crate) const NONE: RunId = RunId(u32::MAX);

    fn new(class: u32, index: u32) -> Self {
        RunId(class << 29 | index)
    }

    fn class(self) -> u32 {
        self.0 >> 29
    }

    fn index(self) -> u32 {
        self.0 & ((1 << 29) - 1)
    }
}

/// The address of one entry: its cell, and its offset within the cell.
///
/// A fixed class splits the 29 bits as 24 of cell and 5 of offset; the overflow
/// class, whose runs are long and few, splits them as 18 and 11. Both halves
/// are checked when a run is allocated, so exhausting either fails loudly
/// rather than aliasing two entries onto one address.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct EntryId(u32);

/// Offset bits per class: fixed classes address 32 entries, the overflow class
/// 2048.
const fn offset_bits(class: u32) -> u32 {
    if class == BIG { 11 } else { 5 }
}

impl EntryId {
    pub(crate) fn from_raw(raw: u32) -> Self {
        EntryId(raw)
    }

    pub(crate) fn raw(self) -> u32 {
        self.0
    }

    fn new(run: RunId, offset: usize) -> Self {
        let bits = offset_bits(run.class());
        EntryId(run.class() << 29 | run.index() << bits | offset as u32)
    }

    fn run(self) -> RunId {
        let bits = offset_bits(self.0 >> 29);
        RunId::new(self.0 >> 29, (self.0 & ((1 << 29) - 1)) >> bits)
    }

    fn offset(self) -> usize {
        (self.0 & ((1 << offset_bits(self.0 >> 29)) - 1)) as usize
    }
}

#[repr(C)]
struct Run<const N: usize> {
    owner: OpId,
    entries: [Entry; N],
}

/// A run above the largest fixed class: one pooled heap block.
struct BigRun {
    owner: OpId,
    entries: Box<[Entry]>,
}

/// Run `$body` against the cell `$run` names, whichever class holds it.
macro_rules! on_run {
    ($runs:expr, $run:expr, $get:ident, |$cell:ident| $body:expr) => {{
        let run = $run;
        let index = run.index();
        match run.class() {
            0 => {
                let $cell = $runs.c2.$get(index).expect("live run");
                $body
            }
            1 => {
                let $cell = $runs.c4.$get(index).expect("live run");
                $body
            }
            2 => {
                let $cell = $runs.c8.$get(index).expect("live run");
                $body
            }
            3 => {
                let $cell = $runs.c16.$get(index).expect("live run");
                $body
            }
            4 => {
                let $cell = $runs.c32.$get(index).expect("live run");
                $body
            }
            _ => {
                let $cell = $runs.big.$get(index).expect("live run");
                $body
            }
        }
    }};
}

/// The five fixed classes plus the overflow class.
#[derive(Default)]
pub(crate) struct Runs {
    c2: Hive<Run<2>>,
    c4: Hive<Run<4>>,
    c8: Hive<Run<8>>,
    c16: Hive<Run<16>>,
    c32: Hive<Run<32>>,
    big: Hive<BigRun>,
}

impl Runs {
    /// Cells the class already holds, for the exhaustion check.
    fn len(&self, class: u32) -> usize {
        match class {
            0 => self.c2.len(),
            1 => self.c4.len(),
            2 => self.c8.len(),
            3 => self.c16.len(),
            4 => self.c32.len(),
            _ => self.big.len(),
        }
    }

    /// The smallest class holding `len` entries.
    fn class_for(len: usize) -> u32 {
        CLASS_CAPACITY
            .iter()
            .position(|capacity| len <= *capacity)
            .map_or(BIG, |class| class as u32)
    }

    /// Store `ids` as one run owned by `owner`, in the smallest class that
    /// holds them. Entries past `ids` are the run's spare capacity.
    pub(crate) fn alloc(&mut self, owner: OpId, ids: &[u32]) -> RunId {
        let class = Self::class_for(ids.len());
        // Refuse before allocating: a rejected run must leave no cell behind.
        assert!(
            ids.len() < 1 << offset_bits(BIG),
            "an operation may hold at most {} ports",
            (1u32 << offset_bits(BIG)) - 1
        );
        assert!(
            self.len(class) < 1 << (29 - offset_bits(class)),
            "run cells exhausted for size class {class}"
        );
        let fill = |entries: &mut [Entry]| {
            for (entry, id) in entries.iter_mut().zip(ids) {
                *entry = Entry::new(*id);
            }
        };
        let index = match class {
            0 => self.c2.insert(Run {
                owner,
                entries: {
                    let mut e = [Entry::new(0); 2];
                    fill(&mut e);
                    e
                },
            }),
            1 => self.c4.insert(Run {
                owner,
                entries: {
                    let mut e = [Entry::new(0); 4];
                    fill(&mut e);
                    e
                },
            }),
            2 => self.c8.insert(Run {
                owner,
                entries: {
                    let mut e = [Entry::new(0); 8];
                    fill(&mut e);
                    e
                },
            }),
            3 => self.c16.insert(Run {
                owner,
                entries: {
                    let mut e = [Entry::new(0); 16];
                    fill(&mut e);
                    e
                },
            }),
            4 => self.c32.insert(Run {
                owner,
                entries: {
                    let mut e = [Entry::new(0); 32];
                    fill(&mut e);
                    e
                },
            }),
            _ => {
                let mut entries = vec![Entry::new(0); ids.len()].into_boxed_slice();
                fill(&mut entries);
                self.big.insert(BigRun { owner, entries })
            }
        };
        RunId::new(class, index)
    }

    pub(crate) fn free(&mut self, run: RunId) {
        let index = run.index();
        match run.class() {
            0 => drop(self.c2.remove(index)),
            1 => drop(self.c4.remove(index)),
            2 => drop(self.c8.remove(index)),
            3 => drop(self.c16.remove(index)),
            4 => drop(self.c32.remove(index)),
            _ => drop(self.big.remove(index)),
        }
    }

    pub(crate) fn owner(&self, run: RunId) -> OpId {
        on_run!(self, run, get, |cell| cell.owner)
    }

    pub(crate) fn entries(&self, run: RunId) -> &[Entry] {
        on_run!(self, run, get, |cell| &cell.entries[..])
    }

    pub(crate) fn entries_mut(&mut self, run: RunId) -> &mut [Entry] {
        on_run!(self, run, get_mut, |cell| &mut cell.entries[..])
    }

    pub(crate) fn capacity(&self, run: RunId) -> usize {
        self.entries(run).len()
    }

    /// The entry at `offset` of `run`, by address.
    pub(crate) fn entry_id(&self, run: RunId, offset: usize) -> EntryId {
        EntryId::new(run, offset)
    }

    pub(crate) fn entry(&self, id: EntryId) -> &Entry {
        &self.entries(id.run())[id.offset()]
    }

    pub(crate) fn entry_mut(&mut self, id: EntryId) -> &mut Entry {
        let (run, offset) = (id.run(), id.offset());
        &mut self.entries_mut(run)[offset]
    }

    /// The op and operand index an entry address names.
    pub(crate) fn locate(&self, id: EntryId) -> (OpId, usize) {
        (self.owner(id.run()), id.offset())
    }

    /// Move `run`'s first `live` entries into a class holding `needed`, freeing
    /// the old cell. Entry addresses change, so the caller relinks.
    pub(crate) fn grow(&mut self, run: RunId, live: usize, needed: usize) -> RunId {
        debug_assert!(live <= needed);
        let owner = self.owner(run);
        let mut ids: Vec<u32> = self.entries(run)[..live].iter().map(|e| e.id).collect();
        // Above the largest fixed class there is no next class to promote to,
        // so capacity doubles instead; without that every push past 32 ports
        // would reallocate and relink, making a long operand list quadratic.
        let needed = if Self::class_for(needed) == BIG {
            needed.max(self.capacity(run) * 2)
        } else {
            needed
        };
        ids.resize(needed, 0);
        let grown = self.alloc(owner, &ids);
        self.entries_mut(grown)[live..needed]
            .iter_mut()
            .for_each(|entry| *entry = Entry::new(0));
        self.free(run);
        grown
    }

    pub(crate) fn recycle(&mut self) {
        self.c2.recycle();
        self.c4.recycle();
        self.c8.recycle();
        self.c16.recycle();
        self.c32.recycle();
        self.big.recycle();
    }

    /// Live cells, allocated chunks and bytes, for the memory census.
    pub(crate) fn census(&self) -> (usize, usize, usize) {
        let hives: [(usize, usize, usize); 6] = [
            (self.c2.len(), self.c2.chunk_count(), self.c2.bytes()),
            (self.c4.len(), self.c4.chunk_count(), self.c4.bytes()),
            (self.c8.len(), self.c8.chunk_count(), self.c8.bytes()),
            (self.c16.len(), self.c16.chunk_count(), self.c16.bytes()),
            (self.c32.len(), self.c32.chunk_count(), self.c32.bytes()),
            (self.big.len(), self.big.chunk_count(), self.big.bytes()),
        ];
        hives.iter().fold((0, 0, 0), |acc, hive| {
            (acc.0 + hive.0, acc.1 + hive.1, acc.2 + hive.2)
        })
    }
}

/// Capacity of each fixed attribute class, by class number.
const ATTR_CAPACITY: [usize; 4] = [1, 2, 4, 8];

/// The cell an op's attributes live in; see [`RunId`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct AttrRunId(u32);

impl AttrRunId {
    pub(crate) const NONE: AttrRunId = AttrRunId(u32::MAX);

    fn new(class: u32, index: u32) -> Self {
        AttrRunId(class << 29 | index)
    }

    fn class(self) -> u32 {
        self.0 >> 29
    }

    fn index(self) -> u32 {
        self.0 & ((1 << 29) - 1)
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

macro_rules! on_attrs {
    ($runs:expr, $run:expr, $get:ident, |$cell:ident| $body:expr) => {{
        let run = $run;
        let index = run.index();
        match run.class() {
            0 => {
                let $cell = $runs.a1.$get(index).expect("live attribute run");
                $body
            }
            1 => {
                let $cell = $runs.a2.$get(index).expect("live attribute run");
                $body
            }
            2 => {
                let $cell = $runs.a4.$get(index).expect("live attribute run");
                $body
            }
            3 => {
                let $cell = $runs.a8.$get(index).expect("live attribute run");
                $body
            }
            _ => {
                let $cell = $runs.big.$get(index).expect("live attribute run");
                $body
            }
        }
    }};
}

/// Attributes get the same treatment as ports: a run in a size-classed hive,
/// so reading one is a single hop and an op with attributes costs no heap block
/// of its own.
#[derive(Default)]
pub(crate) struct AttrRuns {
    a1: Hive<[Attr; 1]>,
    a2: Hive<[Attr; 2]>,
    a4: Hive<[Attr; 4]>,
    a8: Hive<[Attr; 8]>,
    big: Hive<Box<[Attr]>>,
}

impl AttrRuns {
    fn class_for(len: usize) -> u32 {
        ATTR_CAPACITY
            .iter()
            .position(|capacity| len <= *capacity)
            .map_or(BIG, |class| class as u32)
    }

    /// Store `attributes` as one run. An empty list gets no cell at all.
    pub(crate) fn alloc(&mut self, attributes: Vec<Attr>) -> AttrRunId {
        if attributes.is_empty() {
            return AttrRunId::NONE;
        }
        let class = Self::class_for(attributes.len());
        let mut attributes = attributes.into_iter();
        let mut next = || attributes.next().unwrap_or_else(filler);
        let index = match class {
            0 => self.a1.insert(std::array::from_fn(|_| next())),
            1 => self.a2.insert(std::array::from_fn(|_| next())),
            2 => self.a4.insert(std::array::from_fn(|_| next())),
            3 => self.a8.insert(std::array::from_fn(|_| next())),
            _ => self.big.insert(attributes.collect()),
        };
        AttrRunId::new(class, index)
    }

    pub(crate) fn free(&mut self, run: AttrRunId) {
        if run == AttrRunId::NONE {
            return;
        }
        let index = run.index();
        match run.class() {
            0 => drop(self.a1.remove(index)),
            1 => drop(self.a2.remove(index)),
            2 => drop(self.a4.remove(index)),
            3 => drop(self.a8.remove(index)),
            _ => drop(self.big.remove(index)),
        }
    }

    pub(crate) fn get(&self, run: AttrRunId, count: usize) -> &[Attr] {
        if run == AttrRunId::NONE {
            return &[];
        }
        &on_attrs!(self, run, get, |cell| &cell[..])[..count]
    }

    pub(crate) fn get_mut(&mut self, run: AttrRunId, count: usize) -> &mut [Attr] {
        if run == AttrRunId::NONE {
            return &mut [];
        }
        &mut on_attrs!(self, run, get_mut, |cell| &mut cell[..])[..count]
    }

    pub(crate) fn recycle(&mut self) {
        self.a1.recycle();
        self.a2.recycle();
        self.a4.recycle();
        self.a8.recycle();
        self.big.recycle();
    }

    /// Live cells, allocated chunks and bytes, for the memory census.
    pub(crate) fn census(&self) -> (usize, usize, usize) {
        let hives: [(usize, usize, usize); 5] = [
            (self.a1.len(), self.a1.chunk_count(), self.a1.bytes()),
            (self.a2.len(), self.a2.chunk_count(), self.a2.bytes()),
            (self.a4.len(), self.a4.chunk_count(), self.a4.bytes()),
            (self.a8.len(), self.a8.chunk_count(), self.a8.bytes()),
            (self.big.len(), self.big.chunk_count(), self.big.bytes()),
        ];
        hives.iter().fold((0, 0, 0), |acc, hive| {
            (acc.0 + hive.0, acc.1 + hive.1, acc.2 + hive.2)
        })
    }
}
