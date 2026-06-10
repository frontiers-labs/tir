//! Pipeline log in the Kanata format (version 0004) consumed by the Konata
//! viewer (<https://github.com/shioyadan/Konata>). The handler buffers the
//! per-instruction dispatch/issue/retire cycles emitted by the scoreboard and
//! renders them as cycle-ordered commands: each instruction is introduced
//! (`I`) and labeled (`L`, with its PC and disassembly) when dispatched,
//! occupies a `D` (dispatched, waiting to issue) and then `X` (executing)
//! stage, and retires (`R`) in program order.

use std::fmt::Write;

use tir_sim::scoreboard::{EventHandler, SimContext};

pub struct KonataView {
    /// Per-instruction `L` text: PC and disassembly, in trace order.
    labels: Vec<String>,
    dispatch: Vec<u64>,
    issue: Vec<u64>,
    retire: Vec<u64>,
}

impl KonataView {
    pub fn new(labels: Vec<String>) -> Self {
        let n = labels.len();
        KonataView {
            labels,
            dispatch: vec![0; n],
            issue: vec![0; n],
            retire: vec![0; n],
        }
    }
}

impl EventHandler for KonataView {
    fn start(&mut self, ctx: &SimContext) {
        assert_eq!(
            ctx.base.len() * ctx.iterations,
            self.labels.len(),
            "one label per trace instruction"
        );
    }

    fn dispatched(&mut self, cycle: u64, i: usize) {
        self.dispatch[i] = cycle;
    }

    fn issued(&mut self, cycle: u64, i: usize) {
        self.issue[i] = cycle;
    }

    fn retired(&mut self, cycle: u64, i: usize) {
        self.retire[i] = cycle;
    }

    fn render(&self) -> String {
        // Gather each instruction's commands keyed by cycle, then a stable sort
        // groups them into Konata's single forward-moving time domain while
        // keeping same-cycle commands in program order.
        let mut events: Vec<(u64, String)> = Vec::new();
        for (i, label) in self.labels.iter().enumerate() {
            let (d, s, r) = (self.dispatch[i], self.issue[i], self.retire[i]);
            events.push((d, format!("I\t{i}\t{i}\t0")));
            events.push((d, format!("L\t{i}\t0\t{label}")));
            events.push((d, format!("S\t{i}\t0\tD")));
            events.push((s, format!("S\t{i}\t0\tX")));
            // Retirement is in-order, so the retire id equals the index.
            events.push((r, format!("R\t{i}\t{i}\t0")));
        }
        events.sort_by_key(|(cycle, _)| *cycle);

        let mut out = String::from("Kanata\t0004\n");
        let mut cur = events.first().map(|(cycle, _)| *cycle).unwrap_or(0);
        let _ = writeln!(out, "C=\t{cur}");
        for (cycle, line) in events {
            if cycle > cur {
                let _ = writeln!(out, "C\t{}", cycle - cur);
                cur = cycle;
            }
            out.push_str(&line);
            out.push('\n');
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two instructions: the first dispatches/issues at cycle 0 and retires at
    /// 2; the second dispatches at 0, waits a cycle for its operand, and
    /// retires at 4.
    #[test]
    fn renders_cycle_ordered_kanata_log() {
        let mut view = KonataView::new(vec![
            "0x80000000: add a0, a1, a2".to_string(),
            "0x80000004: add a3, a0, a0".to_string(),
        ]);
        view.dispatched(0, 0);
        view.issued(0, 0);
        view.dispatched(0, 1);
        view.issued(1, 1);
        view.retired(2, 0);
        view.retired(4, 1);

        let expected = "\
Kanata\t0004
C=\t0
I\t0\t0\t0
L\t0\t0\t0x80000000: add a0, a1, a2
S\t0\t0\tD
S\t0\t0\tX
I\t1\t1\t0
L\t1\t0\t0x80000004: add a3, a0, a0
S\t1\t0\tD
C\t1
S\t1\t0\tX
C\t1
R\t0\t0\t0
C\t2
R\t1\t1\t0
";
        assert_eq!(view.render(), expected);
    }
}
