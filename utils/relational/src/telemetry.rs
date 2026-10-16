//! Per-round saturation counters: how much of the graph each round searched and
//! how much of that search was already-applied work. Printed as `tir-sat:` lines
//! on stderr under `TIR_TIME_PASSES`, alongside the pass-timing table.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::Stats;
use crate::saturate::Delta;

/// Mirrors the core pass timer's switch; this crate cannot see it.
pub fn enabled() -> bool {
    static FROM_ENV: OnceLock<bool> = OnceLock::new();
    *FROM_ENV.get_or_init(|| std::env::var_os("TIR_TIME_PASSES").is_some_and(|value| value != "0"))
}

fn rule_stats_enabled() -> bool {
    static FROM_ENV: OnceLock<bool> = OnceLock::new();
    *FROM_ENV.get_or_init(|| std::env::var_os("TIR_RULE_STATS").is_some_and(|value| value != "0"))
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
    rebuilds: usize,
    batches: usize,
    batched: usize,
}

thread_local! {
    static RULES: RefCell<BTreeMap<String, RuleCounts>> = const { RefCell::new(BTreeMap::new()) };
    static ROUNDS: RefCell<Vec<Round>> = const { RefCell::new(Vec::new()) };
    static ELAPSED: Cell<Duration> = const { Cell::new(Duration::ZERO) };
    static EXTRACTED: Cell<(usize, Duration)> = const { Cell::new((0, Duration::ZERO)) };
}

#[derive(Default)]
struct RuleCounts {
    applications: usize,
    changed: usize,
}

pub(super) fn register_rules<'a>(names: impl IntoIterator<Item = &'a str>) {
    if rule_stats_enabled() {
        RULES.with(|counts| {
            let mut counts = counts.borrow_mut();
            for name in names {
                counts.entry(name.to_string()).or_default();
            }
        });
    }
}

pub(super) fn count_rule(name: &str, applications: usize, idle: usize) {
    if !rule_stats_enabled() {
        return;
    }
    RULES.with(|counts| {
        let mut counts = counts.borrow_mut();
        let count = counts.get_mut(name).expect("registered saturation rule");
        count.applications += applications;
        count.changed += applications - idle;
    });
}

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

/// One saturation round's counters, or nothing when telemetry is off — every
/// method is then a plain pass-through. The database counts its own work, so a
/// round is the difference across it: live ids and the client's size cannot
/// answer this, since a match may equate two ids and mint two more.
pub struct RoundStats(Option<(Round, Stats)>);

impl RoundStats {
    /// Open a round searching against `delta` (`None` on the first round, which
    /// searches everything).
    pub fn start<G>(stats: Stats, delta: Option<&Delta<G>>) -> Self {
        if !enabled() {
            return Self(None);
        }
        Self(Some((
            Round {
                changed: delta.map_or(usize::MAX, Delta::len),
                ..Round::default()
            },
            stats,
        )))
    }

    /// One rule's root set for this round, drawn from `delta`'s frontier.
    pub fn searched<G>(&mut self, roots: usize, delta: Option<&Delta<G>>) {
        let Some((round, _)) = &mut self.0 else {
            return;
        };
        round.searched += roots;
        round.delta = round.delta.max(delta.map_or(0, Delta::frontier));
    }

    /// Count `found` matches applied, `idle` of them no-ops: they equated
    /// nothing and minted nothing, as re-finding an old match does.
    pub fn applied(&mut self, found: usize, idle: usize) {
        if let Some((round, _)) = &mut self.0 {
            round.found += found;
            round.noop += idle;
        }
    }

    /// Close the round; call after the rebuild, whose merges and repairs it counts.
    pub fn finish(self, now: Stats) {
        let Some((mut round, base)) = self.0 else {
            return;
        };
        round.merged = now.merges - base.merges;
        round.repairs = now.repairs - base.repairs;
        round.rebuilds = now.rebuilds - base.rebuilds;
        round.batches = now.batches - base.batches;
        round.batched = now.batched - base.batched;
        ROUNDS.with(|rounds| rounds.borrow_mut().push(round));
    }
}

/// Print and reset the telemetry recorded since the last call.
pub fn report_saturation(pass: &str) {
    if rule_stats_enabled() {
        RULES.with(|counts| {
            for (name, count) in std::mem::take(&mut *counts.borrow_mut()) {
                eprintln!(
                    "tir-rule: pass={pass} rule={name} applications={} changed={} noop={}",
                    count.applications,
                    count.changed,
                    count.applications - count.changed,
                );
            }
        });
    }
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
         changed=[{}] delta=[{}] searched=[{}] found=[{}] noop=[{}] merged=[{}] repairs=[{}] \
         rebuilds=[{}] batches=[{}] batched=[{}]",
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
        column(|r| r.rebuilds),
        column(|r| r.batches),
        column(|r| r.batched),
    );
}
