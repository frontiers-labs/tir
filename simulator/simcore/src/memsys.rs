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

#[cfg(test)]
mod tests {
    use super::*;

    fn cache(size: u64, ways: u32, line: u64, latency: u64) -> CacheParams {
        CacheParams {
            size,
            ways,
            line,
            latency,
            banks: 1,
            mshrs: 16,
        }
    }

    /// A two-level system: 1 KiB/2-way/lat 2 L1D over a 4 KiB/lat 10 "L2", DRAM 100.
    fn small() -> MemorySystem {
        MemorySystem::new(MemParams {
            l1i: cache(1024, 2, 64, 2),
            l1d: cache(1024, 2, 64, 2),
            l2: Some(cache(4096, 4, 64, 10)),
            l3: None,
            dram_latency: 100,
            dram_streams: 8,
        })
    }

    #[test]
    fn hit_costs_less_than_miss() {
        let mut m = small();
        // Cold miss: the speculative DRAM request departs at probe start, so the
        // completion is the absolute DRAM latency.
        let miss = m.access_data(0, 0x1000, false, 0);
        assert_eq!(miss, 100);
        assert_eq!(m.stats().l1d.misses, 1);
        // Warm hit to the same line: just L1D latency.
        let hit = m.access_data(0, 0x1000, false, 200);
        assert_eq!(hit, 202);
        assert_eq!(m.stats().l1d.hits, 1);
    }

    #[test]
    fn lru_evicts_oldest() {
        let mut m = small();
        // set index = (addr/64) % sets; sets = 1024/64/2 = 8. Lines 0, 8, 16
        // all map to set 0 in a 2-way cache.
        let (a, b, c) = (0x0000u64, 8 * 64u64, 16 * 64u64);
        m.access_data(0, a, false, 0); // fill a
        m.access_data(0, b, false, 200); // fill b (set full: {a,b})
        m.access_data(0, a, false, 400); // touch a -> a is MRU, b is LRU
        m.access_data(0, c, false, 600); // fill c evicts b
        let before = m.stats().l1d.misses;
        // a still resident (hit), b evicted (miss).
        assert_eq!(m.access_data(0, a, false, 800), 802);
        assert_eq!(m.stats().l1d.misses, before, "a must still be cached");
        let miss_b = m.access_data(0, b, false, 1000);
        assert!(miss_b > 1002, "b was evicted, so it misses");
    }

    #[test]
    fn associativity_conflict_evicts() {
        // ways+1 distinct lines in one set force an eviction; the first line
        // then misses again.
        let mut m = small();
        for k in 0..3u64 {
            m.access_data(0, k * 8 * 64, false, k * 200); // all map to set 0
        }
        let miss = m.access_data(0, 0, false, 1000); // line 0 was the LRU victim
        assert!(miss > 1002, "line 0 evicted by the third conflicting line");
    }

    #[test]
    fn mshr_merges_same_line() {
        // A line whose fill is still in flight (its MSHR not yet reclaimed) but
        // whose tag was evicted: a re-access merges onto the outstanding fill
        // rather than issuing a new DRAM request. A direct-mapped tiny L1D makes
        // the eviction deterministic.
        let mut m = MemorySystem::new(MemParams {
            l1i: cache(128, 1, 64, 2),
            l1d: cache(128, 1, 64, 2), // 2 sets, direct-mapped
            l2: Some(cache(1 << 20, 8, 64, 10)),
            l3: None,
            dram_latency: 100,
            dram_streams: 8,
        });
        let a = m.access_data(0, 0, false, 0); // line 0 -> set 0, fills, tracked
        m.access_data(0, 128, false, 0); // line 2 -> set 0, evicts line 0
        assert_eq!(m.stats().dram_accesses, 2);
        // Re-access line 0 while its fill (completion `a`) is still outstanding:
        // tag miss + in-flight MSHR -> merge, no new DRAM traffic.
        let merged = m.access_data(0, 0, false, 0);
        assert_eq!(
            m.stats().dram_accesses,
            2,
            "re-access merges, no new request"
        );
        assert_eq!(merged, a, "merged access rides the outstanding fill");
    }

    #[test]
    fn mshr_full_stalls() {
        // One MSHR at L1D: a second in-flight miss to a different line must wait
        // for the first to complete before it can even start.
        let mut m = MemorySystem::new(MemParams {
            l1i: cache(1024, 2, 64, 2),
            l1d: CacheParams {
                mshrs: 1,
                ..cache(1024, 2, 64, 2)
            },
            l2: Some(cache(1 << 20, 8, 64, 10)),
            l3: None,
            dram_latency: 100,
            dram_streams: 8,
        });
        let first = m.access_data(0, 0x0000, false, 0);
        // Different line, same cycle: the single MSHR is busy until `first`.
        let second = m.access_data(0, 0x4000, false, 0);
        assert!(
            second >= first,
            "second miss stalls on the full MSHR table: {second} vs {first}"
        );
    }

    #[test]
    fn bank_conflict_serializes() {
        // Two banks: same-line accesses share a bank and serialize; a
        // different-bank access does not wait.
        let params = CacheParams {
            banks: 2,
            ..cache(1 << 20, 8, 64, 3)
        };
        let mut m = MemorySystem::new(MemParams {
            l1i: params,
            l1d: params,
            l2: Some(cache(1 << 20, 8, 64, 10)),
            l3: None,
            dram_latency: 100,
            dram_streams: 8,
        });
        // Warm the lines so accesses are L1 hits (isolating the bank effect).
        m.access_data(0, 0, false, 0);
        m.access_data(0, 64, false, 0);
        m.access_data(0, 128, false, 0);
        // Line 0 -> bank 0, line 1 -> bank 1. Same cycle (after the warm-up
        // fills complete), different banks: both start at the same time.
        let a = m.access_data(0, 0, false, 1000);
        let b = m.access_data(0, 64, false, 1000);
        assert_eq!(a, b, "different banks do not conflict");
        // Two accesses to bank 0 (lines 0 and 2) at the same cycle serialize:
        // the second starts one occupancy cycle later.
        let c0 = m.access_data(0, 0, false, 2000);
        let c1 = m.access_data(0, 2 * 64, false, 2000);
        assert_eq!(c1, c0 + 1, "same-bank accesses serialize by one cycle");
    }

    #[test]
    fn write_allocate_dirty_writeback_counted() {
        // A store misses, allocates the line dirty; evicting it later triggers a
        // counted writeback.
        let mut m = small();
        let a = 0u64;
        let b = 8 * 64u64; // same set as a
        let c = 16 * 64u64; // same set
        m.access_data(0, a, true, 0); // store: allocate a dirty
        m.access_data(0, b, false, 200); // set {a(dirty), b}
        m.access_data(0, c, false, 400); // evict LRU a (dirty) -> writeback
        assert_eq!(m.stats().writebacks, 1);
        // The writeback lands at L2 as an extra access.
        assert!(m.stats().l2.accesses >= 4);
    }

    #[test]
    fn inst_and_data_caches_are_separate() {
        let mut m = small();
        // Fill a data line, then fetch the same address as an instruction: the
        // I-cache is cold, so it misses independently.
        m.access_data(0, 0x1000, false, 0);
        let fetch = m.access_inst(0x1000, 100);
        assert!(fetch > 102, "L1I is separate and cold: {fetch}");
        assert_eq!(m.stats().l1i.misses, 1);
        assert_eq!(m.stats().l1i.hits, 0);
    }

    #[test]
    fn multi_level_fill_populates_every_level() {
        // With an L3 present, a cold miss threads L1D -> L2 -> L3 -> DRAM and
        // records one access at each.
        let mut m = MemorySystem::new(MemParams {
            l1i: cache(1024, 2, 64, 2),
            l1d: cache(1024, 2, 64, 2),
            l2: Some(cache(4096, 4, 64, 10)),
            l3: Some(cache(1 << 16, 8, 64, 20)),
            dram_latency: 100,
            dram_streams: 8,
        });
        let c = m.access_data(0, 0x9000, false, 0);
        assert_eq!(c, 100, "speculative lookup: the answering level's latency");
        assert_eq!(m.stats().l1d.misses, 1);
        assert_eq!(m.stats().l2.misses, 1);
        assert_eq!(m.stats().l3.misses, 1);
        assert_eq!(m.stats().dram_accesses, 1);
    }

    #[test]
    fn fetch_stall_only_charges_line_crossings() {
        let mut m = small();
        // First fetch of a line misses: positive stall. It also fetches ahead
        // into the next line.
        let s0 = m.fetch_stall(0x8000, 0);
        assert!(s0 > 0, "cold fetch stalls: {s0}");
        // Sequential fetch in the same 64B line: free.
        assert_eq!(m.fetch_stall(0x8004, 10), 0);
        // Crossing into the next line early rides the in-flight fetch-ahead
        // fill: stalled, but strictly less than the cold miss.
        let s1 = m.fetch_stall(0x8040, 20);
        assert!(s1 > 0 && s1 < s0, "late fetch-ahead: {s1} vs cold {s0}");
        // Crossing after the fetch-ahead fill has landed is free.
        assert_eq!(m.fetch_stall(0x8080, 500), 0, "fetch-ahead hides the miss");
        // Demand counters exclude the fetch-ahead walks.
        assert_eq!(m.stats().l1i.accesses, 3);
    }

    #[test]
    fn next_line_prefetch_cuts_misses() {
        use crate::prefetch::NextLine;
        // Baseline: a sequential eight-line walk over cold memory misses eight
        // times.
        let mut base = small();
        for i in 0..8u64 {
            base.access_data(0x400, i * 64, false, i * 200);
        }
        assert_eq!(base.stats().l1d.misses, 8);

        // With next-line prefetching each successor line is fetched by the
        // preceding access, so only the first line cold-misses.
        let mut m = small();
        m.set_prefetcher(Box::new(NextLine::new(64)));
        for i in 0..8u64 {
            m.access_data(0x400, i * 64, false, i * 200);
        }
        assert_eq!(m.stats().l1d.misses, 1, "only the first line misses");
        assert_eq!(m.stats().l1d.hits, 7);
        assert_eq!(m.stats().prefetch.issued, 8);
        assert_eq!(m.stats().prefetch.useful, 7);
    }

    #[test]
    fn stride_prefetch_learns_and_helps() {
        use crate::prefetch::StrideRpt;
        // A big cache isolates the prefetcher from conflict eviction. One PC
        // striding by 256 B (four lines) reaches steady state and prefetches
        // ahead; later demands hit those lines.
        let big = || {
            MemorySystem::new(MemParams {
                l1i: cache(1 << 16, 4, 64, 2),
                l1d: cache(1 << 16, 4, 64, 2),
                l2: Some(cache(1 << 20, 8, 64, 10)),
                l3: None,
                dram_latency: 100,
                dram_streams: 8,
            })
        };
        let mut m = big();
        m.set_prefetcher(Box::new(StrideRpt::new(64)));
        for i in 0..12u64 {
            m.access_data(0x400, i * 256, false, i * 400);
        }
        assert!(m.stats().prefetch.issued > 0, "steady stride prefetches");
        assert!(m.stats().prefetch.useful > 0, "prefetches are consumed");

        // An irregular stream never reaches steady state, so it prefetches
        // nothing.
        let mut r = big();
        r.set_prefetcher(Box::new(StrideRpt::new(64)));
        for (i, a) in [0x1000u64, 0x5000, 0x2000, 0x9000, 0x3000, 0x8000]
            .into_iter()
            .enumerate()
        {
            r.access_data(0x400, a, false, i as u64 * 400);
        }
        assert_eq!(r.stats().prefetch.issued, 0, "no stride, no prefetch");
    }

    #[test]
    fn late_prefetch_rides_the_fill() {
        use crate::prefetch::NextLine;
        // Multiple banks let the line-1 prefetch proceed independently of the
        // line-0 demand instead of serializing behind it.
        let banked = CacheParams {
            banks: 4,
            ..cache(1024, 2, 64, 2)
        };
        let mut m = MemorySystem::new(MemParams {
            l1i: banked,
            l1d: banked,
            l2: Some(CacheParams {
                banks: 4,
                ..cache(4096, 4, 64, 10)
            }),
            l3: None,
            dram_latency: 100,
            dram_streams: 8,
        });
        m.set_prefetcher(Box::new(NextLine::new(64)));
        // Cold miss on line 0 issues a prefetch of line 1.
        m.access_data(0x400, 0, false, 0);
        // Demand line 1 immediately, while its prefetch fill is still in flight:
        // it hits the eagerly-installed tag but must wait for the fill.
        let done = m.access_data(0x400, 64, false, 1);
        assert_eq!(m.stats().prefetch.useful, 1);
        assert_eq!(m.stats().prefetch.late, 1, "fill still outstanding");
        assert!(
            done < 1 + 112,
            "late demand rides the prefetch fill, not a fresh miss: {done}"
        );
    }

    #[test]
    fn prefetch_dropped_when_resident() {
        use crate::prefetch::NextLine;
        let mut m = small();
        m.set_prefetcher(Box::new(NextLine::new(64)));
        // Demand line 1 first (prefetches line 2), then line 0 (would prefetch
        // line 1 — already resident, so it is dropped without an issue).
        m.access_data(0x400, 64, false, 0);
        m.access_data(0x400, 0, false, 200);
        assert_eq!(
            m.stats().prefetch.issued,
            1,
            "resident target not re-issued"
        );
    }

    #[test]
    fn prefetch_hit_below_l1_not_counted_as_demand() {
        use crate::prefetch::NextLine;
        // Make line 1 L2-resident but L1D-evicted (three same-set L1D lines),
        // then demand line 0 so the next-line prefetch walks to L2 and hits
        // there. Demand counters must stay consistent: a prefetch hit at a
        // lower level is not demand traffic.
        let mut m = small();
        m.access_data(0, 64, false, 0); // line 1 -> L1D set 1, L2
        m.access_data(0, (8 + 1) * 64, false, 200); // set 1
        m.access_data(0, (16 + 1) * 64, false, 400); // set 1: evicts line 1
        m.set_prefetcher(Box::new(NextLine::new(64)));
        m.access_data(0x400, 0, false, 600); // prefetches line 1: L1D miss, L2 hit
        assert_eq!(m.stats().prefetch.issued, 1);
        for s in [m.stats().l1d, m.stats().l2, m.stats().l3, m.stats().l1i] {
            assert_eq!(s.hits + s.misses, s.accesses, "demand counters: {s:?}");
        }
    }

    #[test]
    fn prefetch_dropped_when_mshrs_full() {
        use crate::prefetch::NextLine;
        // A single L1D MSHR is held by the in-flight demand miss, so the
        // prefetch finds the table full and is dropped rather than stalling.
        let mut m = MemorySystem::new(MemParams {
            l1i: cache(1024, 2, 64, 2),
            l1d: CacheParams {
                mshrs: 1,
                ..cache(1024, 2, 64, 2)
            },
            l2: Some(cache(1 << 20, 8, 64, 10)),
            l3: None,
            dram_latency: 100,
            dram_streams: 8,
        });
        m.set_prefetcher(Box::new(NextLine::new(64)));
        let demand = m.access_data(0x400, 0, false, 0);
        assert_eq!(
            m.stats().prefetch.issued,
            0,
            "no MSHR free for the prefetch"
        );
        assert_eq!(demand, 100, "the demand miss is unaffected");
    }
}
