//! Presentation of the pipeline event stream. Each [`EventHandler`] here renders a
//! different report; `--view` selects one. Adding a format means adding a handler,
//! not changing the engine.

use std::fmt::Write;

use clap::ValueEnum;

use tir_sim::scoreboard::{EventHandler, SimContext};

/// The selectable report formats.
#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lower")]
pub enum View {
    /// Totals, IPC, per-instruction cost, and resource pressure.
    Resource,
    /// Per-cycle trace of each instruction through dispatch/execute/retire.
    Timeline,
}

/// Build the handler for a selected view.
pub fn make(view: View) -> Box<dyn EventHandler> {
    match view {
        View::Resource => Box::new(ResourceView::default()),
        View::Timeline => Box::new(TimelineView::default()),
    }
}

struct InstrRow {
    text: String,
    latency: u16,
    rthroughput: u16,
    resources: Vec<&'static str>,
}

/// Resource-utilization report: the default `llvm-mca`-style summary.
#[derive(Default)]
struct ResourceView {
    iterations: usize,
    base_len: usize,
    dispatch_width: u16,
    cycles: u64,
    retired: u64,
    instrs: Vec<InstrRow>,
    resource_names: Vec<&'static str>,
    /// `usage[base_idx][resource]`: accumulated resource-cycles over all iterations.
    usage: Vec<Vec<f64>>,
}

impl EventHandler for ResourceView {
    fn start(&mut self, ctx: &SimContext) {
        self.iterations = ctx.iterations;
        self.base_len = ctx.base.len();
        self.dispatch_width = ctx.model.issue_width.max(1);
        self.resource_names = ctx.model.resources.iter().map(|r| r.name).collect();
        self.instrs = ctx
            .base
            .iter()
            .map(|b| InstrRow {
                text: b.text.clone(),
                latency: b.class.latency,
                rthroughput: b.class.rthroughput,
                resources: b.class.resources.to_vec(),
            })
            .collect();
        self.usage = vec![vec![0.0; self.resource_names.len()]; self.base_len];
    }

    fn issued(&mut self, _cycle: u64, i: usize) {
        let base = i % self.base_len;
        let rthr = f64::from(self.instrs[base].rthroughput.max(1));
        for (r, name) in self.resource_names.iter().enumerate() {
            if self.instrs[base].resources.contains(name) {
                self.usage[base][r] += rthr;
            }
        }
    }

    fn retired(&mut self, _cycle: u64, _i: usize) {
        self.retired += 1;
    }

    fn finish(&mut self, total_cycles: u64) {
        self.cycles = total_cycles;
    }

    fn render(&self) -> String {
        let iters = self.iterations.max(1) as f64;
        let mut out = String::new();
        let ipc = if self.cycles == 0 {
            0.0
        } else {
            self.retired as f64 / self.cycles as f64
        };

        let _ = writeln!(out, "Iterations:        {}", self.iterations);
        let _ = writeln!(out, "Instructions:      {}", self.retired);
        let _ = writeln!(out, "Total Cycles:      {}", self.cycles);
        let _ = writeln!(out, "Dispatch Width:    {}", self.dispatch_width);
        let _ = writeln!(out, "IPC:               {ipc:.2}");
        let _ = writeln!(out);

        let _ = writeln!(out, "Instruction Info:");
        let _ = writeln!(out, "[1]: Latency");
        let _ = writeln!(out, "[2]: RThroughput");
        let _ = writeln!(out);
        let _ = writeln!(out, "[1]    [2]    Instructions:");
        for info in &self.instrs {
            let _ = writeln!(
                out,
                "{:<6} {:<6} {}",
                info.latency, info.rthroughput, info.text
            );
        }
        let _ = writeln!(out);

        if self.resource_names.is_empty() {
            return out;
        }

        let _ = writeln!(out, "Resources:");
        for (i, name) in self.resource_names.iter().enumerate() {
            let _ = writeln!(out, "[{i}]: {name}");
        }
        let _ = writeln!(out);

        let header: String = (0..self.resource_names.len())
            .map(|i| format!("[{i}]   "))
            .collect();

        let _ = writeln!(out, "Resource pressure per iteration:");
        let _ = writeln!(out, "{header}");
        let mut line = String::new();
        for r in 0..self.resource_names.len() {
            let total: f64 = self.usage.iter().map(|row| row[r]).sum();
            let _ = write!(line, "{:<6}", format!("{:.2}", total / iters));
        }
        let _ = writeln!(out, "{}", line.trim_end());
        let _ = writeln!(out);

        let _ = writeln!(out, "Resource pressure by instruction:");
        let _ = writeln!(out, "{header}Instructions:");
        for (i, info) in self.instrs.iter().enumerate() {
            let mut line = String::new();
            for r in 0..self.resource_names.len() {
                let p = self.usage[i][r] / iters;
                let cell = if p == 0.0 {
                    "-".to_string()
                } else {
                    format!("{p:.2}")
                };
                let _ = write!(line, "{cell:<6}");
            }
            let _ = writeln!(out, "{line}{}", info.text);
        }

        out
    }
}

/// How many leading iterations the timeline shows; full traces over many
/// iterations are unreadable and add nothing once the steady state is visible.
const TIMELINE_ITERS: usize = 3;

/// Timeline report: a per-cycle character trace of each instruction through the
/// pipeline (`D` dispatch, `=` wait, `e` execute, `R` retire).
#[derive(Default)]
struct TimelineView {
    base_len: usize,
    show: usize,
    texts: Vec<String>,
    dispatch: Vec<u64>,
    issue: Vec<u64>,
    retire: Vec<u64>,
}

impl EventHandler for TimelineView {
    fn start(&mut self, ctx: &SimContext) {
        self.base_len = ctx.base.len();
        self.texts = ctx.base.iter().map(|b| b.text.clone()).collect();
        self.show = self
            .base_len
            .saturating_mul(ctx.iterations.min(TIMELINE_ITERS));
        self.dispatch = vec![0; self.show];
        self.issue = vec![0; self.show];
        self.retire = vec![0; self.show];
    }

    fn dispatched(&mut self, cycle: u64, i: usize) {
        if i < self.show {
            self.dispatch[i] = cycle;
        }
    }

    fn issued(&mut self, cycle: u64, i: usize) {
        if i < self.show {
            self.issue[i] = cycle;
        }
    }

    fn retired(&mut self, cycle: u64, i: usize) {
        if i < self.show {
            self.retire[i] = cycle;
        }
    }

    fn render(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "Timeline view:");
        if self.show == 0 {
            return out;
        }
        let max_cycle = *self.retire.iter().max().unwrap_or(&0);

        for i in 0..self.show {
            let (d, s, r) = (self.dispatch[i], self.issue[i], self.retire[i]);
            let mut grid = String::with_capacity(max_cycle as usize + 1);
            for c in 0..=max_cycle {
                let ch = if c == d {
                    'D'
                } else if c == r {
                    'R'
                } else if c > d && c < s {
                    '='
                } else if c >= s && c < r {
                    'e'
                } else {
                    ' '
                };
                grid.push(ch);
            }
            let label = format!("[{},{}]", i / self.base_len, i % self.base_len);
            let _ = writeln!(out, "{label:<8} {grid}   {}", self.texts[i % self.base_len]);
        }

        out
    }
}
