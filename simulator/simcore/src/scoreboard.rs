//! The shared cycle-assignment engine ("scoreboard") behind both perf views:
//! the static analyzer (`tir sched`, llvm-mca style — no execution, region
//! repeated N times) and the dynamic trace replay (`isasim --timing`, which
//! replays the instruction stream recorded by the functional executor).
//!
//! Both callers reduce their input to a sequence of [`ScoreboardInstr`]s; the
//! engine assigns dispatch/issue/retire cycles honoring data dependencies
//! (forwarding-aware, reconstructed from physical registers exactly like a
//! renamer would), functional-unit contention, issue width, the reorder-buffer
//! window, in-order vs. out-of-order issue, physical-register-file pressure,
//! and — when branch outcomes are supplied — branch-misprediction redirects.
//!
//! The microarchitecture *structure* (units, latencies, widths) comes from a
//! TMDL-generated [`MachineModel`]; the *dynamics* (window policy, predictor,
//! penalties) are Rust-side knobs in [`TimingConfig`], because sweeping those
//! is the point of the simulator.

use std::collections::{HashMap, HashSet, VecDeque};

use tir::backend::liveness::RegRef;
use tir::backend::regalloc::RegisterInfo;
use tir::backend::sched::{InstrSchedClass, MachineModel};

use crate::MemAccess;
use crate::memsys::MemorySystem;
use crate::predictor::BranchPredictor;

/// One instruction as the engine sees it: its scheduling class, the physical
/// registers it reads/writes, and (in trace mode) its resolved branch outcome.
pub struct ScoreboardInstr {
    /// Rendered text for report views; empty when no report is produced.
    pub text: String,
    pub class: InstrSchedClass,
    pub defs: Vec<(String, u16)>,
    pub uses: Vec<(String, u16)>,
    /// The resolved outcome of a conditional branch, recovered from the
    /// executed trace. `None` for non-branches and in static mode, where no
    /// outcome exists to predict against.
    pub branch: Option<BranchOutcome>,
    /// Program counter, for the front-end instruction-cache query. `0` in static
    /// mode, which never passes a memory system, so it is never consulted.
    pub pc: u64,
    /// Data-memory accesses this instruction performs (trace mode only; empty in
    /// static mode). Drives the memory hierarchy when one is present.
    pub mem: Vec<MemAccess>,
}

/// What a conditional branch actually did, so a predictor can be scored.
#[derive(Debug, Clone, Copy)]
pub struct BranchOutcome {
    pub pc: u64,
    pub target: u64,
    pub taken: bool,
}

/// Filter register references down to physical `(class, index)` keys — the
/// granularity the dependence reconstruction works at. `alias` normalizes the
/// class to its physical register file, so classes that alias the same file
/// index-for-index (e.g. arm64 `GPRsp` vs `GPR`) produce matching keys —
/// without it a load's `GPRsp`-classed base address never depends on the `GPR`
/// write that produced it.
pub fn phys_regs(refs: &[RegRef], alias: Option<&Prf>) -> Vec<(String, u16)> {
    refs.iter()
        .filter_map(|r| match r {
            RegRef::Physical { class, index } => {
                let class = match alias {
                    Some(p) => p.file_of(class.name()).to_string(),
                    None => class.name().to_string(),
                };
                Some((class, *index))
            }
            RegRef::Virtual { .. } => None,
        })
        .collect()
}

/// Knobs the microarchitecture model exposes for experimentation. These are
/// *not* in TMDL by design — sweeping them is the whole point of the Rust
/// engine.
#[derive(Debug, Clone, Copy)]
pub struct TimingConfig {
    /// Issue instructions strictly in program order (in-order core) vs. allow
    /// out-of-order issue bounded only by dependencies, resources, and the
    /// window.
    pub in_order: bool,
    /// Maximum in-flight instructions (reorder-buffer size). `0` means
    /// unbounded.
    pub window: usize,
    /// Front-end refetch penalty, in cycles, charged on a branch
    /// misprediction.
    pub mispredict_penalty: u64,
}

impl TimingConfig {
    /// A reasonable default derived from the model: a core that declares a
    /// `rob` buffer is treated as out-of-order with that window; otherwise
    /// in-order with an unbounded window (the in-order issue constraint is
    /// what serializes it). The mispredict penalty approximates the front-end
    /// refill depth.
    pub fn for_model(model: &MachineModel) -> Self {
        let penalty = if model.pipeline.is_empty() {
            8 // deep out-of-order front end
        } else {
            model.pipeline.len() as u64
        };
        match model.buffer("rob") {
            Some(rob) => Self {
                in_order: false,
                window: rob as usize,
                mispredict_penalty: penalty,
            },
            None => Self {
                in_order: true,
                window: 0,
                mispredict_penalty: penalty,
            },
        }
    }
}

/// Physical-register-file pressure model for a renaming core. Ignored on an
/// in-order core, which does not rename.
pub struct Prf {
    /// Register class name -> physical file it draws from.
    pub class_to_file: HashMap<String, String>,
    /// Physical file name -> number of physical registers.
    pub capacity: HashMap<String, u16>,
}

impl Prf {
    /// Map each register class to its physical file and give each file a
    /// capacity: the machine's declared `reg_file` count, or the architectural
    /// register count of that file as a fallback.
    pub fn for_target(info: &RegisterInfo, model: &MachineModel) -> Self {
        let class_to_file = info
            .classes
            .iter()
            .map(|c| (c.name.to_string(), c.file.to_string()))
            .collect();

        // Architectural register count per file: the number of distinct
        // encoding indices the file's classes name.
        let mut indices: HashMap<&str, HashSet<u16>> = HashMap::new();
        for c in info.classes {
            let set = indices.entry(c.file).or_default();
            for &i in c
                .allocation_order
                .iter()
                .chain(c.reserved)
                .chain(c.caller_saved)
                .chain(c.callee_saved)
                .chain(c.arguments)
                .chain(c.return_values)
            {
                set.insert(i);
            }
        }

        let capacity = indices
            .into_iter()
            .map(|(file, idxs)| {
                let cap = model
                    .reg_file(file)
                    .unwrap_or_else(|| idxs.len().min(u16::MAX as usize) as u16);
                (file.to_string(), cap)
            })
            .collect();

        Prf {
            class_to_file,
            capacity,
        }
    }

    fn file_of<'a>(&'a self, class: &'a str) -> &'a str {
        self.class_to_file
            .get(class)
            .map(String::as_str)
            .unwrap_or(class)
    }
}

/// Static context handed to an [`EventHandler`] before the run, so it can size
/// its tables and copy out whatever per-instruction data it needs to report.
pub struct SimContext<'a> {
    pub model: &'a MachineModel,
    pub iterations: usize,
    pub base: &'a [ScoreboardInstr],
}

/// A consumer of pipeline events. Each implementation renders a different
/// report. The instruction index `i` passed to the per-event hooks is the
/// *global* index in the repeated stream; the region instruction is
/// `i % ctx.base.len()` and the iteration is `i / ctx.base.len()`.
pub trait EventHandler {
    fn start(&mut self, _ctx: &SimContext) {}
    fn dispatched(&mut self, _cycle: u64, _i: usize) {}
    fn issued(&mut self, _cycle: u64, _i: usize) {}
    fn retired(&mut self, _cycle: u64, _i: usize) {}
    /// Branch `i` was mispredicted: it resolved its true direction at `resolved`,
    /// and the front end cannot deliver the correct-path successor until
    /// `redirect` (`resolved` + refetch penalty).
    fn mispredicted(&mut self, _i: usize, _resolved: u64, _redirect: u64) {}
    fn finish(&mut self, _total_cycles: u64) {}
    fn render(&self) -> String;
}

/// The outcome of a scoreboard run.
#[derive(Debug, Clone, Copy)]
pub struct TimingResult {
    pub cycles: u64,
    pub instructions: u64,
    /// Conditional branches whose direction was mispredicted.
    pub mispredicts: u64,
}

impl TimingResult {
    /// Instructions retired per cycle.
    pub fn ipc(&self) -> f64 {
        if self.cycles == 0 {
            0.0
        } else {
            self.instructions as f64 / self.cycles as f64
        }
    }
}

/// The producer→consumer latency between two dependent instructions, honoring
/// the machine's forwarding network and falling back to the producer's latency.
fn edge_latency(
    model: &MachineModel,
    producer: &InstrSchedClass,
    consumer: &InstrSchedClass,
) -> u64 {
    if let (Some(p), Some(c)) = (producer.resources.first(), consumer.resources.first())
        && let Some(f) = model.forward_latency(p, c)
    {
        return u64::from(f);
    }
    u64::from(producer.latency)
}

/// The cycle an instruction's result becomes available, given the cycle it
/// issued. Without a memory system, a fixed per-class latency (the closed-form
/// path the differential test guards). With one, each data access is charged
/// against the hierarchy: a load extends completion to its fill cycle, but never
/// *below* the static latency (a hit must not make an instruction faster than
/// scheduled); a store's access is charged for its bank/MSHR/writeback effects
/// but the instruction still retires at its static latency (a post-retirement
/// store buffer approximation).
fn completion_cycle(
    slot: &ScoreboardInstr,
    issue_cycle: u64,
    mem: Option<&mut MemorySystem>,
) -> u64 {
    let base = issue_cycle + u64::from(slot.class.latency);
    let Some(mem) = mem else {
        return base;
    };
    let mut complete = base;
    for access in &slot.mem {
        let done = mem.access_data(slot.pc, access.addr, access.is_write, issue_cycle);
        if !access.is_write {
            complete = complete.max(done);
        }
    }
    complete
}

/// Reorder-buffer occupancy: the retire cycles of in-flight (dispatched, not yet
/// retired) instructions in program order. Its length is the live window
/// occupancy; because retire cycles are monotonic in program order, the front
/// entry is the oldest in flight and frees first.
type Rob = VecDeque<u64>;

/// Register-file pressure gate: raise the dispatch cycle `d` until enough
/// physical registers are free for `slot`'s definitions, mutating the per-file
/// in-flight FIFOs. A no-op on a core that does not rename (`prf` is `None`).
fn prf_gate(
    d: &mut u64,
    slot: &ScoreboardInstr,
    prf: &Prf,
    inflight: &mut HashMap<String, VecDeque<u64>>,
) {
    let mut need: HashMap<&str, usize> = HashMap::new();
    for (class, _) in &slot.defs {
        *need.entry(prf.file_of(class)).or_default() += 1;
    }
    for (file, need) in need {
        let Some(&cap) = prf.capacity.get(file) else {
            continue;
        };
        let cap = cap as usize;
        let q = inflight.entry(file.to_string()).or_default();
        // Free registers whose allocating instruction has retired by `d`.
        while q.front().is_some_and(|&c| c <= *d) {
            q.pop_front();
        }
        // If still short, advance dispatch to the retire cycle that frees the
        // needed count (clamped: an instruction needing more registers than the
        // file holds cannot be helped).
        if q.len() + need > cap && cap >= need {
            let must_free = q.len() + need - cap;
            if let Some(&free_at) = q.get(must_free - 1) {
                *d = (*d).max(free_at);
            }
            for _ in 0..must_free {
                q.pop_front();
            }
        }
    }
}

/// Assign cycles to `base` repeated `iterations` times against `model`.
///
/// `predictor` scores the branch outcomes carried by the instructions (trace
/// mode); without one, branches cost nothing extra. `prf` enables
/// register-file pressure on a renaming (out-of-order) core. `handler`
/// receives dispatch/issue/retire events for report rendering.
///
/// The engine is cycle-stepped: it advances an explicit monotone `cycle` clock,
/// skipping idle cycles by jumping straight to the next dispatch event. Each
/// step dispatches one instruction in program order once the front-end gates
/// (issue-width pacing, ROB window occupancy, misprediction redirect, and
/// register-file pressure) clear at the current clock, then derives that
/// instruction's issue and retire cycles. Issue cycles are assigned oldest-first
/// (program order), which is load-bearing: functional-unit lanes are reserved in
/// that order, so an older instruction claims its lane before any younger one is
/// considered even when the younger becomes ready earlier.
#[allow(clippy::too_many_arguments)]
pub fn run(
    model: &MachineModel,
    base: &[ScoreboardInstr],
    iterations: usize,
    config: &TimingConfig,
    mut predictor: Option<&mut dyn BranchPredictor>,
    prf: Option<&Prf>,
    mut mem: Option<&mut MemorySystem>,
    mut handler: Option<&mut dyn EventHandler>,
) -> TimingResult {
    if let Some(h) = handler.as_mut() {
        h.start(&SimContext {
            model,
            iterations,
            base,
        });
    }

    let n = base.len().saturating_mul(iterations);
    let width = model.issue_width.max(1) as usize;
    let window = if config.window == 0 {
        usize::MAX
    } else {
        config.window
    };
    // Only a renaming (out-of-order) core is subject to register-file pressure.
    let prf = if config.in_order { None } else { prf };

    // Per-resource "lanes": one free-at-cycle per parallel unit.
    let mut lanes: HashMap<&str, Vec<u64>> = model
        .resources
        .iter()
        .map(|r| (r.name, vec![0u64; r.units.max(1) as usize]))
        .collect();

    let mut dispatch = vec![0u64; n];
    let mut issue = vec![0u64; n];
    let mut retire = vec![0u64; n];
    // When a memory op's real completion exceeds its static latency (a cache
    // miss), the extra readiness cycle its dependents must wait for; `0` means
    // "no extra" (the fixed-latency path), keeping the mem-less run identical.
    let mut result_extra = vec![0u64; n];
    let mut reg_writer: HashMap<(String, u16), usize> = HashMap::new();
    // Per physical file, the retire cycles of in-flight register allocations
    // (FIFO: retire times are monotonic, so the oldest allocation frees first).
    let mut prf_inflight: HashMap<String, VecDeque<u64>> = HashMap::new();
    let mut rob: Rob = VecDeque::new();
    // Earliest cycle the front end may resume after a misprediction redirect.
    let mut redirect: u64 = 0;
    let mut mispredicts: u64 = 0;
    // The simulated clock. Advanced monotonically; between dispatches it skips
    // forward to the next cycle a gate can release rather than spinning.
    let mut cycle: u64 = 0;

    for i in 0..n {
        let slot = &base[i % base.len()];

        // Front end: advance the clock to instruction `i`'s dispatch cycle. It
        // dispatches in program order, at most `width` per cycle, and no earlier
        // than the ROB has a free window slot, the front end has recovered from
        // any misprediction redirect, and enough physical registers are free.
        let mut d = cycle;
        if i >= width {
            d = d.max(dispatch[i - width] + 1);
        }
        // Window: reclaim ROB slots retired by `d`; if the ROB is still full,
        // wait for its oldest in-flight instruction to retire. `rob.len() ==
        // window` holds exactly when the old closed form's `retire[i-window] > d`.
        while rob.front().is_some_and(|&r| r <= d) {
            rob.pop_front();
        }
        if rob.len() >= window {
            d = d.max(*rob.front().unwrap());
            while rob.front().is_some_and(|&r| r <= d) {
                rob.pop_front();
            }
        }
        d = d.max(redirect);
        if let Some(prf) = prf {
            prf_gate(&mut d, slot, prf, &mut prf_inflight);
        }
        // Front-end instruction fetch: only an L1I miss (a new line that missed)
        // stalls dispatch; a hit is folded into the pipeline depth.
        if let Some(mem) = mem.as_deref_mut() {
            d += mem.fetch_stall(slot.pc, d);
        }
        cycle = d;
        dispatch[i] = cycle;
        if let Some(h) = handler.as_mut() {
            h.dispatched(cycle, i);
        }

        // Operands ready: the latest forwarding-aware producer result.
        let mut operands_ready = 0u64;
        for u in &slot.uses {
            if let Some(&j) = reg_writer.get(u) {
                let producer = &base[j % base.len()];
                operands_ready = operands_ready
                    .max(issue[j] + edge_latency(model, &producer.class, &slot.class))
                    .max(result_extra[j]);
            }
        }

        let mut t = cycle.max(operands_ready);
        if config.in_order && i > 0 {
            t = t.max(issue[i - 1]);
        }

        // Functional-unit contention: an instruction can't issue until a lane
        // in each resource it needs is free.
        for r in slot.class.resources {
            if let Some(lane_set) = lanes.get(*r) {
                t = t.max(lane_set.iter().copied().min().unwrap_or(0));
            }
        }
        issue[i] = t;
        if let Some(h) = handler.as_mut() {
            h.issued(t, i);
        }

        // Reserve the earliest-free lane in each used resource for `rthroughput`.
        let busy_until = t + u64::from(slot.class.rthroughput.max(1));
        for r in slot.class.resources {
            if let Some(lane) = lanes
                .get_mut(*r)
                .and_then(|s| s.iter_mut().min_by_key(|c| **c))
            {
                *lane = busy_until;
            }
        }

        for def in &slot.defs {
            reg_writer.insert(def.clone(), i);
        }

        // Branch scoring: compare the predicted direction to the recorded
        // outcome, and stall the front end on a mispredict until the branch
        // resolves plus the refetch penalty.
        if let (Some(p), Some(br)) = (predictor.as_mut(), &slot.branch) {
            let predicted = p.predict(br.pc, br.target);
            if predicted != br.taken {
                mispredicts += 1;
                let resolved = issue[i] + u64::from(slot.class.latency);
                redirect = redirect.max(resolved + config.mispredict_penalty);
                if let Some(h) = handler.as_mut() {
                    h.mispredicted(i, resolved, redirect);
                }
            }
            p.update(br.pc, br.target, br.taken);
        }

        // In-order retire: completes at its (possibly memory-dependent) result
        // cycle, no earlier than its predecessor retires.
        let complete = completion_cycle(slot, issue[i], mem.as_deref_mut());
        // Only a completion that overran the static latency (a cache miss) holds
        // back dependents beyond forwarding; a hit leaves the fast path intact.
        if complete > issue[i] + u64::from(slot.class.latency) {
            result_extra[i] = complete;
        }
        retire[i] = complete.max(if i > 0 { retire[i - 1] } else { 0 });
        if let Some(h) = handler.as_mut() {
            h.retired(retire[i], i);
        }
        rob.push_back(retire[i]);

        if let Some(prf) = prf {
            for (class, _) in &slot.defs {
                let file = prf.file_of(class);
                if prf.capacity.contains_key(file) {
                    prf_inflight
                        .entry(file.to_string())
                        .or_default()
                        .push_back(retire[i]);
                }
            }
        }
    }

    let cycles = retire.last().map(|c| c + 1).unwrap_or(0);
    if let Some(h) = handler.as_mut() {
        h.finish(cycles);
    }
    TimingResult {
        cycles,
        instructions: n as u64,
        mispredicts,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::predictor::AlwaysNotTaken;
    use tir::backend::sched::{Forward, ProcUnit};

    /// The pre-refactor closed-form engine, kept verbatim as the oracle for the
    /// differential test below: the cycle-stepped [`run`] must reproduce it
    /// bit-for-bit (cycle count, mispredicts, and the full event trace) on every
    /// core configuration. It earns its keep as a permanent guard because Stage C
    /// will make load/store latency state-dependent through [`completion_cycle`],
    /// and the default (fixed-latency) path must stay identical.
    fn run_reference(
        model: &MachineModel,
        base: &[ScoreboardInstr],
        iterations: usize,
        config: &TimingConfig,
        mut predictor: Option<&mut dyn BranchPredictor>,
        prf: Option<&Prf>,
        mut handler: Option<&mut dyn EventHandler>,
    ) -> TimingResult {
        if let Some(h) = handler.as_mut() {
            h.start(&SimContext {
                model,
                iterations,
                base,
            });
        }
        let n = base.len().saturating_mul(iterations);
        let width = model.issue_width.max(1) as usize;
        let window = if config.window == 0 {
            usize::MAX
        } else {
            config.window
        };
        let prf = if config.in_order { None } else { prf };
        let mut lanes: HashMap<&str, Vec<u64>> = model
            .resources
            .iter()
            .map(|r| (r.name, vec![0u64; r.units.max(1) as usize]))
            .collect();
        let mut dispatch = vec![0u64; n];
        let mut issue = vec![0u64; n];
        let mut retire = vec![0u64; n];
        let mut reg_writer: HashMap<(String, u16), usize> = HashMap::new();
        let mut prf_inflight: HashMap<String, VecDeque<u64>> = HashMap::new();
        let mut redirect: u64 = 0;
        let mut mispredicts: u64 = 0;
        for i in 0..n {
            let slot = &base[i % base.len()];
            let mut d = if i > 0 { dispatch[i - 1] } else { 0 };
            if i >= width {
                d = d.max(dispatch[i - width] + 1);
            }
            if i >= window {
                d = d.max(retire[i - window]);
            }
            d = d.max(redirect);
            if let Some(prf) = prf {
                prf_gate(&mut d, slot, prf, &mut prf_inflight);
            }
            dispatch[i] = d;
            if let Some(h) = handler.as_mut() {
                h.dispatched(d, i);
            }
            let mut operands_ready = 0u64;
            for u in &slot.uses {
                if let Some(&j) = reg_writer.get(u) {
                    let producer = &base[j % base.len()];
                    operands_ready = operands_ready
                        .max(issue[j] + edge_latency(model, &producer.class, &slot.class));
                }
            }
            let mut t = d.max(operands_ready);
            if config.in_order && i > 0 {
                t = t.max(issue[i - 1]);
            }
            for r in slot.class.resources {
                if let Some(lane_set) = lanes.get(*r) {
                    t = t.max(lane_set.iter().copied().min().unwrap_or(0));
                }
            }
            issue[i] = t;
            if let Some(h) = handler.as_mut() {
                h.issued(t, i);
            }
            let busy_until = t + u64::from(slot.class.rthroughput.max(1));
            for r in slot.class.resources {
                if let Some(lane) = lanes
                    .get_mut(*r)
                    .and_then(|s| s.iter_mut().min_by_key(|c| **c))
                {
                    *lane = busy_until;
                }
            }
            for def in &slot.defs {
                reg_writer.insert(def.clone(), i);
            }
            if let (Some(p), Some(br)) = (predictor.as_mut(), &slot.branch) {
                let predicted = p.predict(br.pc, br.target);
                if predicted != br.taken {
                    mispredicts += 1;
                    let resolved = issue[i] + u64::from(slot.class.latency);
                    redirect = redirect.max(resolved + config.mispredict_penalty);
                    if let Some(h) = handler.as_mut() {
                        h.mispredicted(i, resolved, redirect);
                    }
                }
                p.update(br.pc, br.target, br.taken);
            }
            let complete = issue[i] + u64::from(slot.class.latency);
            retire[i] = complete.max(if i > 0 { retire[i - 1] } else { 0 });
            if let Some(h) = handler.as_mut() {
                h.retired(retire[i], i);
            }
            if let Some(prf) = prf {
                for (class, _) in &slot.defs {
                    let file = prf.file_of(class);
                    if prf.capacity.contains_key(file) {
                        prf_inflight
                            .entry(file.to_string())
                            .or_default()
                            .push_back(retire[i]);
                    }
                }
            }
        }
        let cycles = retire.last().map(|c| c + 1).unwrap_or(0);
        if let Some(h) = handler.as_mut() {
            h.finish(cycles);
        }
        TimingResult {
            cycles,
            instructions: n as u64,
            mispredicts,
        }
    }

    /// Records the full event stream so the two engines' callbacks can be
    /// compared cycle-for-cycle and in order.
    #[derive(Default, PartialEq, Debug)]
    struct Recorder(Vec<(char, u64, u64)>);
    impl EventHandler for Recorder {
        fn dispatched(&mut self, cycle: u64, i: usize) {
            self.0.push(('D', cycle, i as u64));
        }
        fn issued(&mut self, cycle: u64, i: usize) {
            self.0.push(('I', cycle, i as u64));
        }
        fn retired(&mut self, cycle: u64, i: usize) {
            self.0.push(('R', cycle, i as u64));
        }
        fn mispredicted(&mut self, i: usize, resolved: u64, redirect: u64) {
            self.0.push(('M', resolved, i as u64));
            self.0.push(('m', redirect, i as u64));
        }
        fn finish(&mut self, total: u64) {
            self.0.push(('F', total, 0));
        }
        fn render(&self) -> String {
            String::new()
        }
    }

    // A test machine with a shared single-lane resource (`MUL`) so lane-priority
    // corner cases — an older, not-yet-ready instruction reserving the lane
    // ahead of a younger ready one — are actually exercised.
    fn model(issue_width: u16) -> MachineModel {
        let resources: &'static [ProcUnit] = Box::leak(
            vec![
                ProcUnit {
                    name: "ALU",
                    units: 2,
                },
                ProcUnit {
                    name: "MUL",
                    units: 1,
                },
                ProcUnit {
                    name: "LSU",
                    units: 1,
                },
            ]
            .into_boxed_slice(),
        );
        MachineModel {
            name: "diff-test",
            issue_width,
            resources,
            buffers: &[],
            pipeline: &[],
            forwards: &[Forward {
                from: "ALU",
                to: "ALU",
                latency: 1,
            }],
            reg_files: &[],
            sched: &[],
        }
    }

    const CLASSES: &[InstrSchedClass] = &[
        InstrSchedClass::DEFAULT,
        InstrSchedClass {
            latency: 1,
            read_cycle: 0,
            rthroughput: 1,
            resources: &["ALU"],
        },
        InstrSchedClass {
            latency: 3,
            read_cycle: 0,
            rthroughput: 2,
            resources: &["MUL"],
        },
        InstrSchedClass {
            latency: 4,
            read_cycle: 0,
            rthroughput: 1,
            resources: &["LSU"],
        },
    ];

    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
            self.0 >> 16
        }
        fn upto(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    fn gen_program(rng: &mut Lcg, len: usize) -> Vec<ScoreboardInstr> {
        (0..len)
            .map(|k| {
                let class = CLASSES[rng.upto(CLASSES.len() as u64) as usize];
                // Small register pressure to trip RAW deps and the PRF gate.
                let defs = if rng.upto(4) != 0 {
                    vec![("GPR".to_string(), rng.upto(6) as u16)]
                } else {
                    vec![]
                };
                let uses = (0..rng.upto(3))
                    .map(|_| ("GPR".to_string(), rng.upto(6) as u16))
                    .collect();
                let branch = if rng.upto(5) == 0 {
                    Some(BranchOutcome {
                        pc: k as u64,
                        target: rng.next(),
                        taken: rng.upto(2) == 0,
                    })
                } else {
                    None
                };
                ScoreboardInstr {
                    text: String::new(),
                    class,
                    defs,
                    uses,
                    branch,
                    pc: 0,
                    mem: Vec::new(),
                }
            })
            .collect()
    }

    fn prf() -> Prf {
        Prf {
            class_to_file: [("GPR".to_string(), "GPR".to_string())]
                .into_iter()
                .collect(),
            capacity: [("GPR".to_string(), 8u16)].into_iter().collect(),
        }
    }

    /// The cycle-stepped engine must reproduce the closed-form oracle exactly —
    /// same cycles, mispredicts, and event trace — across in-order/out-of-order,
    /// bounded/unbounded window, and with/without register-file pressure, over
    /// many random instruction mixes.
    #[test]
    fn cycle_stepped_matches_closed_form() {
        let mut rng = Lcg(0x1234_5678);
        let prf = prf();
        for width in [1u16, 2, 4] {
            let m = model(width);
            for trial in 0..400 {
                let len = 1 + rng.upto(30) as usize;
                let base = gen_program(&mut rng, len);
                let iterations = 1 + rng.upto(3) as usize;
                for &in_order in &[false, true] {
                    for &win in &[0usize, 4, 16] {
                        for &use_prf in &[false, true] {
                            let cfg = TimingConfig {
                                in_order,
                                window: win,
                                mispredict_penalty: 5,
                            };
                            let prf_arg = if use_prf { Some(&prf) } else { None };

                            let mut p_new = AlwaysNotTaken;
                            let mut ev_new = Recorder::default();
                            let r_new = run(
                                &m,
                                &base,
                                iterations,
                                &cfg,
                                Some(&mut p_new),
                                prf_arg,
                                None,
                                Some(&mut ev_new),
                            );

                            let mut p_ref = AlwaysNotTaken;
                            let mut ev_ref = Recorder::default();
                            let r_ref = run_reference(
                                &m,
                                &base,
                                iterations,
                                &cfg,
                                Some(&mut p_ref),
                                prf_arg,
                                Some(&mut ev_ref),
                            );

                            assert_eq!(
                                (r_new.cycles, r_new.mispredicts, r_new.instructions),
                                (r_ref.cycles, r_ref.mispredicts, r_ref.instructions),
                                "trial {trial} width {width} in_order {in_order} win {win} prf {use_prf}"
                            );
                            assert_eq!(
                                ev_new, ev_ref,
                                "event trace differs: trial {trial} width {width} in_order {in_order} win {win} prf {use_prf}"
                            );
                        }
                    }
                }
            }
        }
    }
}
