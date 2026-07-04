//! Pipeline log in the Kanata format (version 0004) consumed by the Konata
//! viewer (<https://github.com/shioyadan/Konata>). The handler buffers the
//! per-instruction cycles the scoreboard assigns and renders every instruction
//! through the full pipeline, one stage bar per phase:
//!
//! * `F`     — front-end fetch/decode, the cycles before dispatch.
//! * `flush` — a front-end refill: the fetch of the correct-path instruction
//!   after a branch misprediction, stretched across the refetch penalty so the
//!   bubble is visible instead of an unexplained gap.
//! * `D`     — dispatched, waiting in the issue queue for operands/resources.
//! * `X`     — executing (address generation for a memory op).
//! * `M`     — the memory access of a load/store.
//! * `Cm`    — complete, waiting for in-order retire.
//!
//! Each instruction is introduced (`I`) and labeled (`L`) when fetched and
//! retires (`R`) in program order. Mispredicted branches carry a second `L`
//! detail label.
//!
//! Under a mispredicted branch the front end kept fetching down the predicted
//! (wrong) path; those instructions (supplied by [`KonataView::add_speculation`])
//! flow through the pipeline until the branch resolves, then retire with the
//! flush flag (`R … 1`) so Konata greys them — filling what would otherwise be a
//! dead bubble. `I` file ids are reassigned densely in emission order at render
//! time, as Konata requires them strictly sequential.

use std::collections::HashMap;
use std::fmt::Write;

use tir_sim::scoreboard::{EventHandler, SimContext};

/// One instruction on a mispredicted (wrong) path: fetched speculatively, never
/// committed. Carries only what the renderer needs to draw its stages before the
/// branch resolves and squashes it.
pub struct SpecInstr {
    pub label: String,
    pub is_memory: bool,
}

pub struct KonataView {
    /// Per-instruction `L` text: PC and disassembly, in trace order.
    labels: Vec<String>,
    dispatch: Vec<u64>,
    issue: Vec<u64>,
    retire: Vec<u64>,
    /// Execution latency per instruction, to bound the `X`/`M` stage.
    latency: Vec<u64>,
    /// Whether the instruction accesses memory (uses a load/store unit).
    is_memory: Vec<bool>,
    /// Mispredicted branch index -> (cycle it resolved, cycle the front end
    /// resumes fetching the correct path). The window between is the shadow the
    /// wrong-path instructions occupy.
    mispred: HashMap<usize, (u64, u64)>,
    /// Wrong-path instructions fetched under each mispredicted branch, keyed by
    /// the branch's trace index. Populated after the run by `add_speculation`.
    spec: HashMap<usize, Vec<SpecInstr>>,
    /// Front-end depth: how many cycles fetch/decode take ahead of dispatch.
    front_end_depth: u64,
    /// Fetch/issue width: wrong-path instructions arrive this many per cycle.
    width: u64,
}

impl KonataView {
    pub fn new(labels: Vec<String>) -> Self {
        let n = labels.len();
        KonataView {
            labels,
            dispatch: vec![0; n],
            issue: vec![0; n],
            retire: vec![0; n],
            latency: vec![1; n],
            is_memory: vec![false; n],
            mispred: HashMap::new(),
            spec: HashMap::new(),
            front_end_depth: 1,
            width: 1,
        }
    }

    /// The trace indices of branches that mispredicted, for the caller to walk
    /// the corresponding wrong paths.
    pub fn mispredicted_branches(&self) -> Vec<usize> {
        let mut v: Vec<usize> = self.mispred.keys().copied().collect();
        v.sort_unstable();
        v
    }

    /// How many wrong-path instructions a mispredicted branch could fetch before
    /// it resolves: the shadow length in cycles times the fetch width, capped so
    /// a pathological shadow can't explode the log. `0` if nothing is visible.
    pub fn spec_window(&self, branch: usize) -> usize {
        let Some(&(resolved, _)) = self.mispred.get(&branch) else {
            return 0;
        };
        let base = self.dispatch[branch]
            .saturating_sub(self.front_end_depth)
            .saturating_add(1);
        (resolved.saturating_sub(base) * self.width).min(64) as usize
    }

    /// Attach the wrong-path instruction stream a mispredicted branch fetched
    /// before it resolved. Rendered as speculative, squashed at resolve.
    pub fn add_speculation(&mut self, branch: usize, instrs: Vec<SpecInstr>) {
        if !instrs.is_empty() {
            self.spec.insert(branch, instrs);
        }
    }
}

/// An instruction is a memory access when its scheduling class occupies a
/// load/store unit. The unit is named per TMDL machine; these are the
/// conventional spellings across the backends' models.
pub fn is_memory_class(resources: &[&str]) -> bool {
    resources.iter().any(|r| {
        let r = r.to_ascii_uppercase();
        r.contains("LSU") || r.contains("MEM") || r.contains("LOAD") || r.contains("STORE")
    })
}

impl EventHandler for KonataView {
    fn start(&mut self, ctx: &SimContext) {
        assert_eq!(
            ctx.base.len() * ctx.iterations,
            self.labels.len(),
            "one label per trace instruction"
        );
        // Fetch/decode depth: the cycle offset of the execute phase (the stages
        // ahead of it are the front end). Falls back to a shallow default for a
        // model that declares no explicit pipeline.
        self.front_end_depth = ctx
            .model
            .phase_cycle("EX")
            .map(u64::from)
            .filter(|&d| d > 0)
            .unwrap_or(2);
        self.width = u64::from(ctx.model.issue_width.max(1));
        // Trace mode replays the stream once (iterations == 1), so the global
        // index equals the position in `base`.
        for (i, slot) in ctx.base.iter().enumerate() {
            self.latency[i] = u64::from(slot.class.latency).max(1);
            self.is_memory[i] = is_memory_class(slot.class.resources);
        }
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

    fn mispredicted(&mut self, i: usize, resolved: u64, redirect: u64) {
        self.mispred.insert(i, (resolved, redirect));
    }

    fn render(&self) -> String {
        // Emit each instruction's stage transitions keyed by cycle, then a stable
        // sort merges them into Konata's single forward-moving time domain while
        // keeping same-cycle commands in program order.
        let mut events: Vec<(u64, String)> = Vec::new();
        for i in 0..self.labels.len() {
            let d = self.dispatch[i];
            let s = self.issue[i];
            let r = self.retire[i];
            let lat = self.latency[i];
            let complete = s + lat;

            // Fetch begins `front_end_depth` cycles before dispatch. But if the
            // preceding instruction was a mispredicted branch, this is the
            // correct-path successor: it cannot be fetched until the branch
            // resolved, so its fetch spans the whole front-end refill (the gap
            // that would otherwise look like an inexplicable stall).
            let refill = i
                .checked_sub(1)
                .and_then(|p| self.mispred.get(&p))
                .map(|&(resolved, _)| resolved);
            let fetch_start = refill.unwrap_or_else(|| d.saturating_sub(self.front_end_depth));
            let fetch_stage = if refill.is_some() { "flush" } else { "F" };

            events.push((fetch_start, format!("I\t{i}\t{i}\t0")));
            events.push((fetch_start, format!("L\t{i}\t0\t{}", self.labels[i])));
            if self.mispred.contains_key(&i) {
                events.push((fetch_start, format!("L\t{i}\t1\tmispredicted branch")));
            }
            events.push((fetch_start, format!("S\t{i}\t0\t{fetch_stage}")));
            events.push((d, format!("S\t{i}\t0\tD")));

            // Execute, splitting off the memory access of a load/store so it
            // reads as address-generation (`X`) then memory (`M`).
            if self.is_memory[i] && lat >= 2 {
                events.push((s, format!("S\t{i}\t0\tX")));
                events.push((s + 1, format!("S\t{i}\t0\tM")));
            } else if self.is_memory[i] {
                events.push((s, format!("S\t{i}\t0\tM")));
            } else {
                events.push((s, format!("S\t{i}\t0\tX")));
            }

            // Done executing but blocked behind older instructions retiring.
            if r > complete {
                events.push((complete, format!("S\t{i}\t0\tCm")));
            }
            events.push((r, format!("R\t{i}\t{i}\t0")));
        }

        // Wrong-path speculation. Under each mispredicted branch, the front end
        // kept fetching down the predicted (wrong) path; those instructions flow
        // through the pipeline until the branch resolves, then are squashed
        // (retired with the flush flag, so Konata greys them out). They fill what
        // would otherwise be a dead bubble between the branch and its refill.
        let mut spec_id = self.labels.len();
        for &branch in self.mispredicted_branches().iter() {
            let Some(instrs) = self.spec.get(&branch) else {
                continue;
            };
            let (resolved, _redirect) = self.mispred[&branch];
            // Wrong-path fetch starts the cycle after the branch is fetched, and
            // delivers `width` instructions per cycle.
            let base = self.dispatch[branch]
                .saturating_sub(self.front_end_depth)
                .saturating_add(1);
            for (k, sp) in instrs.iter().enumerate() {
                let f = base + (k as u64) / self.width;
                if f >= resolved {
                    break; // the branch resolves before this one could be fetched
                }
                let id = spec_id;
                spec_id += 1;
                events.push((f, format!("I\t{id}\t{id}\t0")));
                events.push((f, format!("L\t{id}\t0\t{}", sp.label)));
                events.push((f, format!("L\t{id}\t1\tspeculative (wrong path)")));
                events.push((f, format!("S\t{id}\t0\tF")));
                // Advance through the pipeline as far as the shadow allows.
                let dc = (f + self.front_end_depth).min(resolved);
                if dc > f {
                    events.push((dc, format!("S\t{id}\t0\tD")));
                }
                let xc = (dc + 1).min(resolved);
                if xc > dc {
                    let stage = if sp.is_memory { "M" } else { "X" };
                    events.push((xc, format!("S\t{id}\t0\t{stage}")));
                }
                // Squashed when the branch resolves: retire with the flush flag.
                events.push((resolved, format!("R\t{id}\t0\t1")));
            }
        }

        events.sort_by_key(|(cycle, _)| *cycle);

        let mut out = String::from("Kanata\t0004\n");
        let mut cur = events.first().map(|(cycle, _)| *cycle).unwrap_or(0);
        let _ = writeln!(out, "C=\t{cur}");
        // Konata requires the `I` command's first field (INSN_ID_IN_FILE) to be
        // strictly sequential in file order — it is the handle every later command
        // references. Our internal ids aren't: speculative ids are allocated last
        // but fetched (and so emitted) early. Reassign a dense file id to each
        // instruction at its `I` line and remap every command onto it.
        let mut next_fid = 0u64;
        let mut fid: HashMap<u64, u64> = HashMap::new();
        for (cycle, line) in events {
            if cycle > cur {
                let _ = writeln!(out, "C\t{}", cycle - cur);
                cur = cycle;
            }
            let mut parts = line.splitn(4, '\t');
            let cmd = parts.next().unwrap_or("");
            let id: u64 = parts.next().unwrap_or("0").parse().unwrap_or(0);
            let f2 = parts.next().unwrap_or("");
            let f3 = parts.next().unwrap_or("");
            if cmd == "I" {
                let f = next_fid;
                next_fid += 1;
                fid.insert(id, f);
                // Second field (INSN_ID_IN_SIM) is display-only; reuse the file id
                // so the shown sequence number climbs monotonically. Third is the
                // thread id.
                let _ = writeln!(out, "I\t{f}\t{f}\t{f3}");
            } else {
                let f = fid.get(&id).copied().unwrap_or(id);
                let _ = writeln!(out, "{cmd}\t{f}\t{f2}\t{f3}");
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a view whose per-instruction metadata is set directly, bypassing
    /// `start` (which needs a `SimContext`/`MachineModel`).
    fn view(labels: &[&str], depth: u64) -> KonataView {
        let mut v = KonataView::new(labels.iter().map(|s| s.to_string()).collect());
        v.front_end_depth = depth;
        v
    }

    /// Two ALU instructions through fetch/dispatch/execute/commit. The first
    /// issues immediately and retires at 2; the second waits a cycle for its
    /// operand and retires at 4, sitting in `Cm` until then.
    #[test]
    fn renders_full_pipeline_stages() {
        let mut v = view(
            &["0x80000000: add a0, a1, a2", "0x80000004: add a3, a0, a0"],
            2,
        );
        v.dispatched(2, 0);
        v.issued(2, 0);
        v.retired(3, 0);
        v.dispatched(3, 1);
        v.issued(4, 1);
        v.retired(5, 1);

        let out = v.render();
        assert!(out.starts_with("Kanata\t0004\nC=\t0\n"));
        // Instruction 0: fetched at 0 (dispatch 2 - depth 2), D at 2, X at 2, R at 3.
        assert!(out.contains("S\t0\t0\tF"));
        assert!(out.contains("S\t0\t0\tD"));
        assert!(out.contains("S\t0\t0\tX"));
        assert!(out.contains("R\t0\t0\t0"));
        // Instruction 1 completes at 5 (issue 4 + lat 1) == retire, so no Cm.
        assert!(!out.contains("S\t1\t0\tCm"));
    }

    /// A load shows a distinct memory stage, and a completed instruction that
    /// waits to retire shows `Cm`.
    #[test]
    fn memory_and_commit_wait() {
        let mut v = view(&["0x80000000: lw a0, 0(a1)"], 1);
        v.latency[0] = 3;
        v.is_memory[0] = true;
        v.dispatched(1, 0);
        v.issued(1, 0);
        v.retired(10, 0); // completes at 4, then stalls to 10
        let out = v.render();
        assert!(out.contains("S\t0\t0\tX")); // address generation
        assert!(out.contains("S\t0\t0\tM")); // memory access
        assert!(out.contains("S\t0\t0\tCm")); // waiting to retire
    }

    /// A mispredicted branch stretches its successor's fetch back to the branch
    /// resolve cycle and tags it as a `flush`, with a detail label on the branch.
    #[test]
    fn mispredict_shows_flush_refill() {
        let mut v = view(&["0x80000000: bnez a0, .L", "0x80000004: add a1, a1, 1"], 2);
        v.dispatched(0, 0);
        v.issued(0, 0);
        v.retired(1, 0);
        v.mispredicted(0, 1, 9); // resolved at 1, redirect at 9
        v.dispatched(9, 1);
        v.issued(9, 1);
        v.retired(10, 1);
        let out = v.render();
        // Branch is annotated.
        assert!(out.contains("L\t0\t1\tmispredicted branch"));
        // Successor's fetch is a flush spanning from the resolve cycle (1).
        assert!(out.contains("S\t1\t0\tflush"));
        // The flush fetch is emitted at the branch-resolve cycle, an 8-cycle
        // refill before the successor dispatches — not at `dispatch - depth`.
        let flush_pos = out.find("S\t1\t0\tflush").unwrap();
        let d_pos = out.find("S\t1\t0\tD").unwrap();
        assert!(out[flush_pos..d_pos].contains("C\t8"));
    }

    /// Wrong-path instructions attached to a mispredicted branch fill the shadow
    /// and are squashed (retired with the flush flag) at the resolve cycle.
    #[test]
    fn speculation_fills_and_squashes() {
        let mut v = view(&["0x80000000: bnez a0, .L", "0x80000010: add a1, a1, 1"], 2);
        v.width = 2;
        v.dispatched(0, 0);
        v.issued(0, 0);
        v.retired(1, 0);
        v.mispredicted(0, 6, 14); // resolves at 6, refills to 14
        v.dispatched(14, 1);
        v.issued(14, 1);
        v.retired(15, 1);
        // A five-cycle shadow at width 2 leaves room for wrong-path fetches.
        assert!(v.spec_window(0) >= 2);
        v.add_speculation(
            0,
            vec![
                SpecInstr {
                    label: "0x80000100: mul x1, x2, x3".into(),
                    is_memory: false,
                },
                SpecInstr {
                    label: "0x80000104: ldr x4, [x1]".into(),
                    is_memory: true,
                },
            ],
        );
        let out = v.render();
        // Both wrong-path instructions appear, tagged speculative.
        assert_eq!(out.matches("speculative (wrong path)").count(), 2);
        assert!(out.contains("0x80000100: mul x1, x2, x3"));
        assert!(out.contains("0x80000104: ldr x4, [x1]"));
        // The memory wrong-path op shows the memory stage (the committed ops here
        // are not memory ops, so any `M` stage is speculative).
        assert!(
            out.lines()
                .any(|l| l.starts_with("S\t") && l.ends_with("\tM"))
        );
        // Both are squashed (flush flag), never committed: two `R … 1` lines.
        let squashed = out
            .lines()
            .filter(|l| l.starts_with("R\t") && l.ends_with("\t0\t1"))
            .count();
        assert_eq!(squashed, 2);
        // File ids are dense and sequential (Konata requirement).
        let mut fids: Vec<u64> = out
            .lines()
            .filter_map(|l| l.strip_prefix("I\t"))
            .filter_map(|r| r.split('\t').next())
            .map(|s| s.parse().unwrap())
            .collect();
        let n = fids.len();
        fids.sort_unstable();
        assert_eq!(fids, (0..n as u64).collect::<Vec<_>>());
    }
}
