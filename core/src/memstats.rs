//! Memory instrumentation, off unless `TIR_MEM_STATS` is set in the environment
//! or [`enable`] is called (`fcc --mem-report`).
//!
//! Every report is one labelled line on stderr, `tir-mem: <kind> key=value ...`,
//! so a run can be grepped and diffed across stages. When disabled, each report
//! point costs one atomic load.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use tir_relational::{Engine, Label as ENode};

static FORCED: AtomicBool = AtomicBool::new(false);
static PASS_TOTALS: Mutex<Vec<(&'static str, i64)>> = Mutex::new(Vec::new());

/// Turn reporting on for the rest of the process, as `fcc --mem-report` does.
pub fn enable() {
    FORCED.store(true, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    static FROM_ENV: OnceLock<bool> = OnceLock::new();
    FORCED.load(Ordering::Relaxed)
        || *FROM_ENV
            .get_or_init(|| std::env::var_os("TIR_MEM_STATS").is_some_and(|value| value != "0"))
}

/// Peak and current resident set in kB, as reported by the kernel. Zero on
/// platforms without `/proc`.
fn sample() -> (u64, u64) {
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
        return (0, 0);
    };
    let field = |name: &str| -> u64 {
        status
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|value| value.parse().ok())
            .unwrap_or(0)
    };
    (field("VmHWM:"), field("VmRSS:"))
}

/// Slab capacities against live-entity counts; the gap is unreclaimed storage.
#[derive(Debug, Default, Clone, Copy)]
pub struct SlabCensus {
    pub ops_slab: usize,
    pub ops_live: usize,
    pub values_slab: usize,
    pub values_live: usize,
    pub blocks_slab: usize,
    pub blocks_live: usize,
    pub regions_slab: usize,
    pub regions_live: usize,
    /// Per-arena heap: `Arc` header plus entity struct plus the capacity of the
    /// `Vec`s it owns. Attribute payloads (`Str`, `Array`, `Dict`) are not
    /// chased, so these are estimates for ranking, like [`egraph_census`].
    pub ops_bytes: usize,
    pub values_bytes: usize,
    pub blocks_bytes: usize,
    pub regions_bytes: usize,
    pub slab_bytes: usize,
}

/// Straddles one pass run; [`PassScope::finish`] emits the delta and census.
pub struct PassScope {
    name: &'static str,
    hwm: u64,
    rss: u64,
}

/// `None` when reporting is off, so callers pay nothing.
pub fn pass_scope(name: &'static str) -> Option<PassScope> {
    if !enabled() {
        return None;
    }
    let (hwm, rss) = sample();
    Some(PassScope { name, hwm, rss })
}

impl PassScope {
    pub fn finish(self, census: SlabCensus) {
        let (hwm, rss) = sample();
        let hwm_delta = hwm as i64 - self.hwm as i64;
        eprintln!(
            "tir-mem: pass name={} vmhwm_kb={hwm} hwm_delta_kb={hwm_delta} rss_delta_kb={}",
            self.name,
            rss as i64 - self.rss as i64,
        );
        eprintln!(
            "tir-mem: context after={} ops_slab={} ops_live={} values_slab={} values_live={} \
             blocks_slab={} blocks_live={} regions_slab={} regions_live={} ops_bytes={} \
             values_bytes={} blocks_bytes={} regions_bytes={} slab_bytes={} bytes_per_live_op={}",
            self.name,
            census.ops_slab,
            census.ops_live,
            census.values_slab,
            census.values_live,
            census.blocks_slab,
            census.blocks_live,
            census.regions_slab,
            census.regions_live,
            census.ops_bytes,
            census.values_bytes,
            census.blocks_bytes,
            census.regions_bytes,
            census.slab_bytes,
            census.ops_bytes / census.ops_live.max(1),
        );
        let mut totals = PASS_TOTALS.lock().unwrap();
        match totals.iter_mut().find(|(name, _)| *name == self.name) {
            Some(entry) => entry.1 += hwm_delta,
            None => totals.push((self.name, hwm_delta)),
        }
    }
}

/// How many analysis results the pass pipeline is holding after a pass. Entries
/// are replaced when the IR they describe changes, so this counts live results,
/// not stale ones.
pub fn analysis_census(pass: &'static str, entries: usize) {
    if !enabled() {
        return;
    }
    eprintln!("tir-mem: analyses after={pass} entries={entries}");
}

/// Node and class counts of a view, plus the bytes its columns hold. An estimate
/// for ranking, not an allocator total.
pub fn egraph_census<L: ENode>(label: &str, egraph: &Engine<L>) {
    if !enabled() {
        return;
    }
    let nodes = egraph.total_size();
    let classes = egraph.num_classes();
    let bytes = egraph.approx_bytes();
    eprintln!("tir-mem: egraph label={label} nodes={nodes} classes={classes} approx_bytes={bytes}");
}

/// One PBQP solve: problem size and the bytes its edge matrices occupy. Called
/// once per solve, so a regalloc spill retry shows up as another line.
pub fn pbqp_census(label: &str, nodes: usize, edges: usize, matrix_bytes: usize) {
    if !enabled() {
        return;
    }
    eprintln!(
        "tir-mem: pbqp label={label} nodes={nodes} edges={edges} matrix_bytes={matrix_bytes}"
    );
}

/// Process peak and the per-pass peak deltas, worst first.
pub fn summary() {
    if !enabled() {
        return;
    }
    let (hwm, _) = sample();
    eprintln!("tir-mem: summary peak_vmhwm_kb={hwm}");
    let mut totals = PASS_TOTALS.lock().unwrap().clone();
    totals.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    for (name, delta) in totals {
        eprintln!("tir-mem: top-pass name={name} hwm_delta_kb={delta}");
    }
}
