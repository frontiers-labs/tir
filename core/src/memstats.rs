//! Memory instrumentation, off unless `TIR_MEM_STATS` is set in the environment
//! or [`enable`] is called (`fcc --mem-report`).
//!
//! Every report is one labelled line on stderr, `tir-mem: <kind> key=value ...`,
//! so a run can be grepped and diffed across stages. When disabled, each report
//! point costs one atomic load.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

static FORCED: AtomicBool = AtomicBool::new(false);

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

/// Process peak and current resident memory.
pub fn summary() {
    if enabled() {
        let (hwm, rss) = sample();
        eprintln!("tir-mem: summary vmhwm_kb={hwm} vmrss_kb={rss}");
    }
}
