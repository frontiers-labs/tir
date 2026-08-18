use tir_sim::prefetch::*;

#[test]
fn next_line_predicts_successor() {
    let mut p = NextLine::new(64);
    assert_eq!(p.on_access(0, 0x100, false), vec![0x140]);
    // Fires on hits too, keeping the fetch ahead of the stream.
    assert_eq!(p.on_access(0, 0x100, true), vec![0x140]);
    // Unaligned demand still yields the next aligned line.
    assert_eq!(p.on_access(0, 0x104, false), vec![0x140]);
}

#[test]
fn stride_reaches_steady_and_prefetches() {
    let mut p = StrideRpt::new(64);
    let pc = 0x400;
    // First access: train tag, no stride yet.
    assert!(p.on_access(pc, 0x1000, false).is_empty());
    // Second: stride 256 observed but unconfirmed (Transient).
    assert!(p.on_access(pc, 0x1100, false).is_empty());
    // Third: stride 256 confirmed -> Steady, prefetch +256 and +512.
    assert_eq!(p.on_access(pc, 0x1200, false), vec![0x1300, 0x1400]);
    assert_eq!(p.on_access(pc, 0x1300, false), vec![0x1400, 0x1500]);
}

#[test]
fn stride_ignores_irregular() {
    let mut p = StrideRpt::new(64);
    let pc = 0x400;
    let addrs = [0x1000u64, 0x1200, 0x1180, 0x1500, 0x1080];
    let mut issued = 0;
    for a in addrs {
        issued += p.on_access(pc, a, false).len();
    }
    assert_eq!(issued, 0, "no constant stride, no prefetch");
}

#[test]
fn stride_zero_no_prefetch() {
    let mut p = StrideRpt::new(64);
    let pc = 0x400;
    p.on_access(pc, 0x1000, false);
    // Repeated same address: stride 0, never prefetches.
    for _ in 0..4 {
        assert!(p.on_access(pc, 0x1000, false).is_empty());
    }
}

#[test]
fn by_name_selects_prefetchers() {
    assert!(prefetcher_by_name("none", 64).unwrap().is_none());
    assert_eq!(
        prefetcher_by_name("next-line", 64).unwrap().unwrap().name(),
        "next-line"
    );
    assert_eq!(
        prefetcher_by_name("stride", 64).unwrap().unwrap().name(),
        "stride"
    );
    assert!(prefetcher_by_name("nope", 64).is_err());
}
