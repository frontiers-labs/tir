//! A stateful, cycle-approximate memory hierarchy for the timing engine. Where
//! the scoreboard charges a fixed per-class latency for every load, this model
//! makes load/store completion *state dependent*: set-associative caches with
//! LRU replacement, per-bank contention, a bounded miss-status table (MSHRs),
//! and a DRAM tier with limited memory-level parallelism.
//!
//! It is deliberately approximate. The engine feeds it accesses keyed to issue
//! cycles that are *usually* nondecreasing but can be locally out of order on an
//! out-of-order core; the internal clock is clamped monotone (`now =
//! max(now, cycle)`) and everything is evaluated against `now`. Lookups are
//! *speculative*: each level forwards a request downward at probe start and
//! cancels it on a hit (as ARM cores do), so a level's `latency` is its absolute
//! load-to-use latency and hardware-measured plateaus plug in directly. Several
//! second-order effects are simplified — see the notes on writebacks and bank
//! occupancy below.

use crate::prefetch::Prefetcher;
use std::collections::VecDeque;

/// One cache level's geometry and timing.
#[derive(Debug, Clone, Copy)]
pub struct CacheParams {
    pub size: u64,
    pub ways: u32,
    pub line: u64,
    pub latency: u64,
    pub banks: u32,
    pub mshrs: u32,
}

/// The whole hierarchy: split L1, a shared L2, an optional L3, then DRAM.
#[derive(Debug, Clone)]
pub struct MemParams {
    pub l1i: CacheParams,
    pub l1d: CacheParams,
    pub l2: Option<CacheParams>,
    pub l3: Option<CacheParams>,
    pub dram_latency: u64,
    /// Maximum outstanding DRAM requests (memory-level parallelism).
    pub dram_streams: u32,
}

/// Access counters for one cache level. `hits + misses == accesses` always holds
/// (a writeback is modeled as a hit access at the level it lands in).
#[derive(Debug, Default, Clone, Copy)]
pub struct LevelStats {
    pub accesses: u64,
    pub hits: u64,
    pub misses: u64,
}

/// Prefetch effectiveness counters. `issued` is speculative fills started;
/// `useful` is demand accesses that hit a prefetched line before evicting it;
/// `late` is the subset of those whose fill was still in flight at the hit (the
/// prefetch helped but arrived too close to the demand).
#[derive(Debug, Default, Clone, Copy)]
pub struct PrefetchStats {
    pub issued: u64,
    pub useful: u64,
    pub late: u64,
}

/// Aggregate statistics for a run. L2/L3 stay zero when absent. Cache-level and
/// DRAM counters track *demand* traffic only; prefetch traffic is accounted
/// separately in `prefetch`.
#[derive(Debug, Default, Clone, Copy)]
pub struct MemStats {
    pub l1i: LevelStats,
    pub l1d: LevelStats,
    pub l2: LevelStats,
    pub l3: LevelStats,
    pub dram_accesses: u64,
    pub writebacks: u64,
    pub prefetch: PrefetchStats,
}

/// One resident cache line.
#[derive(Debug, Clone, Copy)]
struct Entry {
    line: u64,
    dirty: bool,
    /// Installed by a prefetch and not yet demanded: the first demand hit clears
    /// it and counts the prefetch as useful.
    prefetched: bool,
}

/// A single set-associative cache level. Tags per set are kept in LRU order
/// (front = least-recently-used); banks and the MSHR table gate concurrency.
#[derive(Debug, Clone)]
struct Cache {
    ways: usize,
    line: u64,
    latency: u64,
    sets: usize,
    /// Cycles a bank stays busy per access: one cycle at L1 (a fast probe),
    /// the level's own latency below it (a coarse stand-in for its access
    /// occupancy). Chosen to be defensible rather than exact.
    occupancy: u64,
    tags: Vec<Vec<Entry>>,
    banks: Vec<u64>,
    mshr_cap: usize,
    /// In-flight misses: (line address, completion cycle).
    mshrs: VecDeque<(u64, u64)>,
}

impl Cache {
    fn new(p: CacheParams, occupancy: u64) -> Self {
        let ways = p.ways.max(1) as usize;
        let sets = ((p.size / p.line.max(1)) as usize / ways).max(1);
        let banks = p.banks.max(1) as usize;
        Cache {
            ways,
            line: p.line.max(1),
            latency: p.latency,
            sets,
            occupancy,
            tags: vec![Vec::with_capacity(ways); sets],
            banks: vec![0; banks],
            mshr_cap: p.mshrs.max(1) as usize,
            mshrs: VecDeque::new(),
        }
    }

    fn line_of(&self, addr: u64) -> u64 {
        addr / self.line
    }

    /// Serialize on the addressed bank: an access starts when the bank frees,
    /// which then stays busy for `occupancy`.
    fn bank_wait(&mut self, addr: u64, arrive: u64) -> u64 {
        let bank = (self.line_of(addr) as usize) % self.banks.len();
        let start = arrive.max(self.banks[bank]);
        self.banks[bank] = start + self.occupancy;
        start
    }

    /// Probe tags. On a hit, promote the line to MRU (and mark it dirty on a
    /// write). Returns whether it hit.
    fn probe(&mut self, addr: u64, is_write: bool) -> bool {
        let line = self.line_of(addr);
        let set = &mut self.tags[(line as usize) % self.sets];
        if let Some(pos) = set.iter().position(|e| e.line == line) {
            let mut e = set.remove(pos);
            e.dirty |= is_write;
            set.push(e);
            true
        } else {
            false
        }
    }

    /// Install `line` (write-allocate: dirty when the demand was a store),
    /// evicting the LRU way if the set is full. `prefetched` tags the line as
    /// speculatively fetched. Returns the evicted line's address when it was
    /// dirty (a writeback the caller must account for).
    fn install(&mut self, addr: u64, is_write: bool, prefetched: bool) -> Option<u64> {
        let line = self.line_of(addr);
        let set = &mut self.tags[(line as usize) % self.sets];
        if let Some(pos) = set.iter().position(|e| e.line == line) {
            set[pos].dirty |= is_write;
            let e = set.remove(pos);
            set.push(e);
            return None;
        }
        let mut evicted = None;
        if set.len() >= self.ways {
            let victim = set.remove(0);
            if victim.dirty {
                evicted = Some(victim.line * self.line);
            }
        }
        set.push(Entry {
            line,
            dirty: is_write,
            prefetched,
        });
        evicted
    }

    /// If `addr`'s line is resident and prefetched-but-undemanded, clear the flag
    /// and report it (the demand that consumes a prefetch). Assumes a prior hit.
    fn take_prefetched(&mut self, addr: u64) -> bool {
        let line = self.line_of(addr);
        let set = &mut self.tags[(line as usize) % self.sets];
        set.iter_mut()
            .find(|e| e.line == line)
            .map(|e| std::mem::replace(&mut e.prefetched, false))
            .unwrap_or(false)
    }

    /// Drop MSHR entries whose fill has completed by `now`.
    fn reclaim(&mut self, now: u64) {
        self.mshrs.retain(|&(_, c)| c > now);
    }

    /// The completion of an in-flight miss to the same line, if any (a merge:
    /// the second miss rides the first's fill, generating no new traffic).
    fn inflight(&self, addr: u64) -> Option<u64> {
        let line = self.line_of(addr);
        self.mshrs
            .iter()
            .find(|&&(l, _)| l == line)
            .map(|&(_, c)| c)
    }

    /// Reserve an MSHR slot for a new miss, stalling the start until the
    /// earliest in-flight fill completes when the table is full.
    fn reserve(&mut self, start: u64) -> u64 {
        if self.mshrs.len() < self.mshr_cap {
            return start;
        }
        let (pos, &(_, earliest)) = self
            .mshrs
            .iter()
            .enumerate()
            .min_by_key(|&(_, &(_, c))| c)
            .unwrap();
        self.mshrs.remove(pos);
        start.max(earliest)
    }

    fn track(&mut self, addr: u64, completion: u64) {
        self.mshrs.push_back((self.line_of(addr), completion));
    }
}

/// The four cache slots, addressed by index for the descent walk.
const L1I: usize = 0;
const L1D: usize = 1;

/// The stateful memory hierarchy. See the module docs for the accuracy caveats.
pub struct MemorySystem {
    /// `[L1I, L1D, L2?, L3?]`. Both L1s share the first lower level.
    caches: Vec<Cache>,
    /// Instruction/data fetch paths through `caches`, top-down.
    inst_path: Vec<usize>,
    data_path: Vec<usize>,
    dram_latency: u64,
    dram_streams: usize,
    /// In-flight DRAM requests: (line, completion).
    dram: VecDeque<(u64, u64)>,
    dram_line: u64,
    now: u64,
    /// The line last fetched by the front end, so sequential fetches into the
    /// same L1I line cost nothing (see [`MemorySystem::fetch_stall`]).
    last_inst_line: Option<u64>,
    /// Optional data prefetcher trained on the demand access stream.
    prefetcher: Option<Box<dyn Prefetcher>>,
    stats: MemStats,
}

impl MemorySystem {
    pub fn new(params: MemParams) -> Self {
        // L1 probes are single-cycle; lower levels occupy a bank for their own
        // access latency (a coarse occupancy stand-in).
        let mut caches = vec![Cache::new(params.l1i, 1), Cache::new(params.l1d, 1)];
        let mut shared = Vec::new();
        if let Some(l2) = params.l2 {
            shared.push(Cache::new(l2, l2.latency));
        }
        if let Some(l3) = params.l3 {
            shared.push(Cache::new(l3, l3.latency));
        }
        // Both L1s descend into the first shared level; shared levels chain
        // downward; the last falls through to DRAM.
        let first_shared = caches.len();
        let dram_line = shared.last().map(|c| c.line).unwrap_or(params.l1d.line);
        caches.extend(shared);
        let inst_path = std::iter::once(L1I)
            .chain(first_shared..caches.len())
            .collect();
        let data_path = std::iter::once(L1D)
            .chain(first_shared..caches.len())
            .collect();
        MemorySystem {
            caches,
            inst_path,
            data_path,
            dram_latency: params.dram_latency,
            dram_streams: params.dram_streams.max(1) as usize,
            dram: VecDeque::new(),
            dram_line: dram_line.max(1),
            now: 0,
            last_inst_line: None,
            prefetcher: None,
            stats: MemStats::default(),
        }
    }

    pub fn stats(&self) -> &MemStats {
        &self.stats
    }

    /// The L1D line size, for aligning prefetch addresses.
    pub fn line(&self) -> u64 {
        self.caches[L1D].line
    }

    /// Attach a data prefetcher, trained on demand accesses in [`access_data`].
    pub fn set_prefetcher(&mut self, prefetcher: Box<dyn Prefetcher>) {
        self.prefetcher = Some(prefetcher);
    }

    /// Complete a data access (load or store), returning the cycle its line is
    /// available in L1D. `pc` trains the prefetcher, which then issues
    /// speculative fills competing for the same banks/MSHRs.
    pub fn access_data(&mut self, pc: u64, addr: u64, is_write: bool, cycle: u64) -> u64 {
        self.now = self.now.max(cycle);
        let path = self.data_path.clone();
        let hits_before = self.stats.l1d.hits;
        let done = self.walk(&path, addr, is_write, false).unwrap();
        let hit = self.stats.l1d.hits > hits_before;
        if let Some(mut pf) = self.prefetcher.take() {
            for target in pf.on_access(pc, addr, hit) {
                if self.walk(&path, target, false, true).is_some() {
                    self.stats.prefetch.issued += 1;
                }
            }
            self.prefetcher = Some(pf);
        }
        done
    }

    /// Complete an instruction fetch, returning the cycle the line is available
    /// in L1I.
    pub fn access_inst(&mut self, pc: u64, cycle: u64) -> u64 {
        self.now = self.now.max(cycle);
        self.walk(&self.inst_path.clone(), pc, false, false)
            .unwrap()
    }

    /// Front-end fetch cost: query the I-cache only when `pc` crosses into a new
    /// line (sequential fetches into a resident line are free). A hit is folded
    /// into the pipeline depth and returns `0`; only a miss returns the extra
    /// cycles the front end stalls beyond an ordinary hit.
    pub fn fetch_stall(&mut self, pc: u64, cycle: u64) -> u64 {
        let line_size = self.caches[L1I].line;
        let line = pc / line_size;
        if self.last_inst_line == Some(line) {
            return 0;
        }
        self.last_inst_line = Some(line);
        let hit_latency = self.caches[L1I].latency;
        let completion = self.access_inst(pc, cycle);
        // Fetch-ahead: hardware front ends always prefetch the next sequential
        // line, hiding lower-level latency for straight-line code. Dropped when
        // resident, in flight, or the MSHRs are full; excluded from demand
        // counters like any prefetch walk.
        self.walk(&self.inst_path.clone(), (line + 1) * line_size, false, true);
        completion.saturating_sub(cycle + hit_latency)
    }

    /// Descend `path` (top-down) for `addr`, returning the completion cycle at
    /// the top level. Lookups are speculative (as on ARM cores): each level
    /// forwards the request downward at probe start, before its own tag check
    /// resolves, and cancels it on a hit — so the level that answers determines
    /// the completion outright, and a level's `latency` is its *absolute*
    /// load-to-use latency, not an increment over the levels above. Levels that
    /// missed install the line on the way back up at no extra cost.
    ///
    /// A `prefetch` walk is a speculative fill: it consumes banks, MSHRs and DRAM
    /// streams like a demand miss but is excluded from the demand hit/miss/DRAM
    /// counters, marks its installed L1D line prefetched, and is *dropped*
    /// (returns `None`) rather than stalling when the line is already resident,
    /// already in flight, or the MSHR table is full. Demand walks never drop.
    fn walk(&mut self, path: &[usize], addr: u64, is_write: bool, prefetch: bool) -> Option<u64> {
        // Levels that truly missed (and so must be installed on the ascent),
        // paired with the cycle their downward request departed.
        let mut missed: Vec<(usize, u64)> = Vec::new();
        let arrive = self.now;
        let mut fill_ready = loop {
            let depth = missed.len();
            if depth == path.len() {
                break self.dram_access(addr, arrive_of(&missed, arrive), prefetch);
            }
            let idx = path[depth];
            let demand = depth == 0;
            let start = self.caches[idx].bank_wait(addr, arrive_of(&missed, arrive));
            if !prefetch {
                self.level_mut(idx).accesses += 1;
            }
            if self.caches[idx].probe(addr, is_write && demand) {
                if prefetch && demand {
                    return None; // resident: drop the prefetch
                }
                if !prefetch {
                    self.level_mut(idx).hits += 1;
                }
                let done = start + self.caches[idx].latency;
                let inflight = self.caches[idx].inflight(addr);
                // A demand hit on a prefetched L1D line consumes the prefetch;
                // if its fill is still in flight the prefetch was late.
                if idx == L1D && self.caches[idx].take_prefetched(addr) {
                    self.stats.prefetch.useful += 1;
                    if inflight.is_some_and(|fill| fill > done) {
                        self.stats.prefetch.late += 1;
                    }
                }
                // Tags are installed when the miss departs, so a hit may land on
                // a line whose fill is still in flight (e.g. a late prefetch):
                // it completes no earlier than the fill.
                break match inflight {
                    Some(fill) if fill > done => fill,
                    _ => done,
                };
            }
            if !prefetch {
                self.level_mut(idx).misses += 1;
            }
            self.caches[idx].reclaim(self.now);
            if let Some(completion) = self.caches[idx].inflight(addr) {
                if prefetch && demand {
                    return None; // already in flight: drop the prefetch
                }
                // Merge into the in-flight miss: no new downward traffic, and
                // the line will be installed by the original miss.
                break completion;
            }
            if prefetch && self.caches[idx].mshrs.len() >= self.caches[idx].mshr_cap {
                return None; // MSHR table full: drop rather than stall
            }
            let start = self.caches[idx].reserve(start);
            missed.push((idx, start));
        };

        // Ascend: install each missed level. The fill was produced by the
        // speculatively-started lower-level request, so installation adds no
        // latency of its own.
        for &(idx, _) in missed.iter().rev() {
            let completion = fill_ready;
            let demand = idx == path[0];
            let mark = prefetch && demand;
            if let Some(evicted) = self.caches[idx].install(addr, is_write && demand, mark) {
                if !prefetch {
                    self.stats.writebacks += 1;
                }
                self.writeback(path, idx, evicted, prefetch);
            }
            self.caches[idx].track(addr, completion);
            fill_ready = completion;
        }
        Some(fill_ready)
    }

    /// A dirty eviction from `idx` writes back into the immediately lower level
    /// (or DRAM). Modeled as a hit access there — under an inclusive hierarchy
    /// the line is resident — occupying a bank but not delaying any demand fill.
    /// Simplification: writeback contention beyond one bank cycle is ignored.
    fn writeback(&mut self, path: &[usize], idx: usize, evicted_addr: u64, prefetch: bool) {
        let pos = path.iter().position(|&p| p == idx).unwrap();
        // A writeback off the last cache drains to DRAM, counted only in
        // `writebacks`; otherwise it lands in the next level down.
        if let Some(&lower) = path.get(pos + 1) {
            self.caches[lower].bank_wait(evicted_addr, self.now);
            if !prefetch {
                let s = self.level_mut(lower);
                s.accesses += 1;
                s.hits += 1;
            }
        }
    }

    /// A DRAM request for `addr`'s line, bounded to `dram_streams` outstanding.
    /// A prefetch consumes a stream slot but is not counted as demand traffic.
    fn dram_access(&mut self, addr: u64, arrive: u64, prefetch: bool) -> u64 {
        let line = addr / self.dram_line;
        self.dram.retain(|&(_, c)| c > self.now);
        if let Some(&(_, c)) = self.dram.iter().find(|&&(l, _)| l == line) {
            return c; // merge onto an outstanding request
        }
        if !prefetch {
            self.stats.dram_accesses += 1;
        }
        let mut start = arrive;
        if self.dram.len() >= self.dram_streams {
            let (pos, &(_, earliest)) = self
                .dram
                .iter()
                .enumerate()
                .min_by_key(|&(_, &(_, c))| c)
                .unwrap();
            self.dram.remove(pos);
            start = start.max(earliest);
        }
        let completion = start + self.dram_latency;
        self.dram.push_back((line, completion));
        completion
    }

    fn level_mut(&mut self, idx: usize) -> &mut LevelStats {
        match idx {
            L1I => &mut self.stats.l1i,
            L1D => &mut self.stats.l1d,
            2 => &mut self.stats.l2,
            _ => &mut self.stats.l3,
        }
    }
}

/// The cycle a fill request departs for the next level down: the start of the
/// deepest miss so far, or the original arrival at the top.
fn arrive_of(missed: &[(usize, u64)], arrive: u64) -> u64 {
    missed.last().map(|&(_, s)| s).unwrap_or(arrive)
}
