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
}

/// What a conditional branch actually did, so a predictor can be scored.
#[derive(Debug, Clone, Copy)]
pub struct BranchOutcome {
    pub pc: u64,
    pub target: u64,
    pub taken: bool,
}

/// Filter register references down to physical `(class, index)` keys — the
/// granularity the dependence reconstruction works at.
pub fn phys_regs(refs: &[RegRef]) -> Vec<(String, u16)> {
    refs.iter()
        .filter_map(|r| match r {
            RegRef::Physical { class, index } => Some((class.clone(), *index)),
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

/// Assign cycles to `base` repeated `iterations` times against `model`.
///
/// `predictor` scores the branch outcomes carried by the instructions (trace
/// mode); without one, branches cost nothing extra. `prf` enables
/// register-file pressure on a renaming (out-of-order) core. `handler`
/// receives dispatch/issue/retire events for report rendering.
pub fn run(
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
    let mut reg_writer: HashMap<(String, u16), usize> = HashMap::new();
    // Per physical file, the retire cycles of in-flight register allocations
    // (FIFO: retire times are monotonic, so the oldest allocation frees first).
    let mut prf_inflight: HashMap<String, VecDeque<u64>> = HashMap::new();
    // Earliest cycle the front end may resume after a misprediction redirect.
    let mut redirect: u64 = 0;
    let mut mispredicts: u64 = 0;

    for i in 0..n {
        let slot = &base[i % base.len()];

        // Front end: in-order dispatch, at most `width` per cycle, bounded by
        // the window (can't dispatch until the instruction `window` slots back
        // retires) and by any outstanding misprediction redirect.
        let mut d = if i > 0 { dispatch[i - 1] } else { 0 };
        if i >= width {
            d = d.max(dispatch[i - width] + 1);
        }
        if i >= window {
            d = d.max(retire[i - window]);
        }
        d = d.max(redirect);

        // Register-file pressure: a renaming core stalls dispatch until enough
        // physical registers free up for this instruction's definitions.
        if let Some(prf) = prf {
            let mut need: HashMap<&str, usize> = HashMap::new();
            for (class, _) in &slot.defs {
                *need.entry(prf.file_of(class)).or_default() += 1;
            }
            for (file, need) in need {
                let Some(&cap) = prf.capacity.get(file) else {
                    continue;
                };
                let cap = cap as usize;
                let q = prf_inflight.entry(file.to_string()).or_default();
                // Free registers whose allocating instruction has retired by `d`.
                while q.front().is_some_and(|&c| c <= d) {
                    q.pop_front();
                }
                // If still short, advance dispatch to the retire cycle that
                // frees the needed count (clamped: an instruction needing more
                // registers than the file holds cannot be helped).
                if q.len() + need > cap && cap >= need {
                    let must_free = q.len() + need - cap;
                    if let Some(&free_at) = q.get(must_free - 1) {
                        d = d.max(free_at);
                    }
                    for _ in 0..must_free {
                        q.pop_front();
                    }
                }
            }
        }
        dispatch[i] = d;
        if let Some(h) = handler.as_mut() {
            h.dispatched(d, i);
        }

        // Operands ready: the latest forwarding-aware producer result.
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
            }
            p.update(br.pc, br.target, br.taken);
        }

        // In-order retire: completes at issue + latency, no earlier than its
        // predecessor retires.
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
