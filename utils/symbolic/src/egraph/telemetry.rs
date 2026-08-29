//! Per-round saturation counters: how much of the graph each round searched and
//! how much of that search was already-applied work. Printed as `tir-sat:` lines
//! on stderr under `TIR_TIME_PASSES`, alongside the pass-timing table.

use std::cell::{Cell, RefCell};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use super::{Delta, EGraph, ENode};

/// Mirrors the core pass timer's switch; this crate cannot see it.
pub fn enabled() -> bool {
    static FROM_ENV: OnceLock<bool> = OnceLock::new();
    *FROM_ENV.get_or_init(|| std::env::var_os("TIR_TIME_PASSES").is_some_and(|value| value != "0"))
}

#[derive(Default, Clone, Copy)]
struct Round {
    changed: usize,
    delta: usize,
    searched: usize,
    found: usize,
    noop: usize,
    merged: usize,
    repairs: usize,
}

thread_local! {
    static ROUNDS: RefCell<Vec<Round>> = const { RefCell::new(Vec::new()) };
    static MERGED: Cell<usize> = const { Cell::new(0) };
    static ADDED: Cell<usize> = const { Cell::new(0) };
    static REPAIRS: Cell<usize> = const { Cell::new(0) };
    static ELAPSED: Cell<Duration> = const { Cell::new(Duration::ZERO) };
    static EXTRACTED: Cell<(usize, Duration)> = const { Cell::new((0, Duration::ZERO)) };
}

/// Record one [`EGraph::extract_best`](super::EGraph::extract_best): it costs a
/// pass over every class, and instcombine runs one per region, so the count is as
/// interesting as the time.
pub(super) fn count_extract(elapsed: Duration) {
    if enabled() {
        EXTRACTED.with(|total| {
            let (runs, time) = total.get();
            total.set((runs + 1, time + elapsed));
        });
    }
}

/// Wall time inside a saturation driver. The pass timer measures a whole pass —
/// seeding, saturation, extraction and rewiring — so the saturation gates of the
/// plan need this narrower number to be judged on.
pub struct Timer(Option<Instant>);

impl Timer {
    pub fn start() -> Self {
        Self(enabled().then(Instant::now))
    }

    pub fn finish(self) {
        if let Some(started) = self.0 {
            ELAPSED.with(|total| total.set(total.get() + started.elapsed()));
        }
    }
}

/// Merges and classes minted so far this round. `num_classes`/`total_size` cannot
/// answer this: inside a scope they read a [`ScopeView`](super::EGraph) snapshot
/// that only refreshes at rebuild, so they hold still while a round applies.
fn effects() -> (usize, usize) {
    (MERGED.with(Cell::get), ADDED.with(Cell::get))
}

pub(super) fn count_merge() {
    if enabled() {
        MERGED.with(|count| count.set(count.get() + 1));
    }
}

pub(super) fn count_add() {
    if enabled() {
        ADDED.with(|count| count.set(count.get() + 1));
    }
}

pub(super) fn count_repair() {
    if enabled() {
        REPAIRS.with(|count| count.set(count.get() + 1));
    }
}

/// One saturation round's counters, or nothing when telemetry is off — every
/// method is then a plain pass-through.
pub struct RoundStats(Option<Round>);

impl RoundStats {
    /// Open a round searching against `delta` (`None` on the first round, which
    /// searches everything).
    pub fn start(delta: Option<&Delta>) -> Self {
        if !enabled() {
            return Self(None);
        }
        MERGED.with(|count| count.set(0));
        ADDED.with(|count| count.set(0));
        REPAIRS.with(|count| count.set(0));
        Self(Some(Round {
            changed: delta.map_or(usize::MAX, Delta::len),
            ..Round::default()
        }))
    }

    /// One rule's root set for this round, drawn from `delta`'s frontier.
    pub fn searched(&mut self, roots: usize, delta: Option<&Delta>) {
        let Some(round) = &mut self.0 else { return };
        round.searched += roots;
        round.delta = round.delta.max(delta.map_or(0, Delta::frontier));
    }

    /// Apply one match, counting it as a no-op if it merged nothing and minted
    /// nothing — the match was already in the graph, as re-finding an old one is.
    pub fn apply<L: ENode>(&mut self, eg: &mut EGraph<L>, apply: impl FnOnce(&mut EGraph<L>)) {
        let Some(round) = &mut self.0 else {
            return apply(eg);
        };
        round.found += 1;
        let before = effects();
        apply(eg);
        round.noop += usize::from(effects() == before);
    }

    /// Close the round; call after the rebuild, whose merges and repairs it counts.
    pub fn finish(self) {
        let Some(mut round) = self.0 else { return };
        round.merged = MERGED.with(Cell::take);
        round.repairs = REPAIRS.with(Cell::take);
        ROUNDS.with(|rounds| rounds.borrow_mut().push(round));
    }
}

/// Print and reset the rounds recorded since the last call, one `tir-sat:` line
/// per reporting caller. A no-op unless `TIR_TIME_PASSES` is set.
pub fn report_saturation(pass: &str) {
    if !enabled() {
        return;
    }
    let extracts = EXTRACTED.replace((0, Duration::ZERO));
    let rounds = ROUNDS.with(|rounds| std::mem::take(&mut *rounds.borrow_mut()));
    if rounds.is_empty() {
        return;
    }
    // `changed` is the previous round's change log; the first round of each
    // saturation has none and searches everything, printed as `all`.
    let column = |pick: fn(&Round) -> usize| {
        let cells: Vec<String> = rounds
            .iter()
            .map(|round| match pick(round) {
                usize::MAX => "all".to_string(),
                value => value.to_string(),
            })
            .collect();
        cells.join(",")
    };
    eprintln!(
        "tir-sat: pass={pass} rounds={} saturate_ms={:.3} extracts={} extract_ms={:.3} \
         changed=[{}] delta=[{}] searched=[{}] found=[{}] noop=[{}] merged=[{}] repairs=[{}]",
        rounds.len(),
        ELAPSED.replace(Duration::ZERO).as_secs_f64() * 1e3,
        extracts.0,
        extracts.1.as_secs_f64() * 1e3,
        column(|r| r.changed),
        column(|r| r.delta),
        column(|r| r.searched),
        column(|r| r.found),
        column(|r| r.noop),
        column(|r| r.merged),
        column(|r| r.repairs),
    );
}
