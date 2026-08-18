use tir_sim::memsys::*;

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
    use tir_sim::prefetch::NextLine;
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
    use tir_sim::prefetch::StrideRpt;
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
    use tir_sim::prefetch::NextLine;
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
    use tir_sim::prefetch::NextLine;
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
    use tir_sim::prefetch::NextLine;
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
    use tir_sim::prefetch::NextLine;
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
