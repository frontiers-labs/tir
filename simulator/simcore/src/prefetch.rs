//! Hardware data prefetchers for the memory hierarchy. A prefetcher observes the
//! demand data-access stream (after the L1D probe, so it knows hit vs. miss) and
//! proposes line-aligned addresses to fetch ahead of demand. The memory system
//! issues them as speculative fills that compete for banks and MSHRs; see
//! [`crate::memsys`] for how issued/useful/late are accounted.

/// A data prefetcher. New policies implement this in Rust and are selected by
/// name; no TMDL involved.
pub trait Prefetcher {
    /// Observe a demand access (post-L1D-probe) and return the line-aligned
    /// addresses to prefetch (a small set, typically 0-2).
    fn on_access(&mut self, pc: u64, addr: u64, hit: bool) -> Vec<u64>;

    fn name(&self) -> &'static str;
}

/// Next-line (one-block-lookahead): on any demand access to line L, prefetch
/// L+1. Prefetching on hits as well as misses lets the fetch stay one line ahead
/// of a sequential stream (drops it to a single cold miss); already-resident
/// L+1s are dropped by the memory system. Cheap and effective on streaming;
/// useless on large strides. Drops are cheap, so a prefetched line's own later
/// demand keeps the chain running.
#[derive(Debug, Clone, Copy)]
pub struct NextLine {
    line: u64,
}

impl NextLine {
    pub fn new(line: u64) -> Self {
        NextLine { line: line.max(1) }
    }
}

impl Prefetcher for NextLine {
    fn on_access(&mut self, _pc: u64, addr: u64, _hit: bool) -> Vec<u64> {
        vec![(addr / self.line + 1) * self.line]
    }

    fn name(&self) -> &'static str {
        "next-line"
    }
}

/// Confidence of a [`StrideRpt`] entry, tracking how many times its recorded
/// stride has repeated. Only `Steady` prefetches.
#[derive(Debug, Clone, Copy, PartialEq)]
enum State {
    Initial,
    Transient,
    Steady,
}

#[derive(Debug, Clone, Copy)]
struct RptEntry {
    tag: u64,
    last_addr: u64,
    stride: i64,
    state: State,
}

/// A Reference Prediction Table (Chen & Baer): one entry per PC (direct-mapped),
/// each learning that PC's constant access stride. When the stride is confirmed
/// (Steady) it prefetches the next two strides ahead, catching strided patterns
/// that next-line misses. Irregular PCs never reach Steady and issue nothing.
#[derive(Debug, Clone)]
pub struct StrideRpt {
    line: u64,
    table: Vec<Option<RptEntry>>,
}

impl StrideRpt {
    const ENTRIES: usize = 64;

    pub fn new(line: u64) -> Self {
        StrideRpt {
            line: line.max(1),
            table: vec![None; Self::ENTRIES],
        }
    }

    fn slot(pc: u64) -> usize {
        (pc as usize / 4) % Self::ENTRIES
    }
}

impl Prefetcher for StrideRpt {
    fn on_access(&mut self, pc: u64, addr: u64, _hit: bool) -> Vec<u64> {
        let slot = Self::slot(pc);
        let entry = &mut self.table[slot];
        let Some(e) = entry.filter(|e| e.tag == pc) else {
            *entry = Some(RptEntry {
                tag: pc,
                last_addr: addr,
                stride: 0,
                state: State::Initial,
            });
            return Vec::new();
        };
        let new_stride = addr as i64 - e.last_addr as i64;
        let correct = new_stride != 0 && new_stride == e.stride;
        let state = match (e.state, correct) {
            (_, true) => State::Steady,
            (State::Initial, false) => State::Transient,
            (State::Transient, false) => State::Transient,
            (State::Steady, false) => State::Initial,
        };
        *entry = Some(RptEntry {
            tag: pc,
            last_addr: addr,
            stride: new_stride,
            state,
        });
        if state == State::Steady && new_stride != 0 {
            let s = new_stride as u64;
            let aligned = |a: u64| a / self.line * self.line;
            return vec![
                aligned(addr.wrapping_add(s)),
                aligned(addr.wrapping_add(s.wrapping_mul(2))),
            ];
        }
        Vec::new()
    }

    fn name(&self) -> &'static str {
        "stride"
    }
}

/// Construct a prefetcher by name for CLI selection, given the L1D line size the
/// prefetch addresses are aligned to. `none` disables prefetching. Mirrors
/// [`crate::predictor::by_name`].
pub fn prefetcher_by_name(name: &str, line: u64) -> Result<Option<Box<dyn Prefetcher>>, String> {
    match name {
        "none" => Ok(None),
        "next-line" => Ok(Some(Box::new(NextLine::new(line)))),
        "stride" => Ok(Some(Box::new(StrideRpt::new(line)))),
        _ => Err(format!(
            "unknown prefetcher '{name}' (expected: none, next-line, stride)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
