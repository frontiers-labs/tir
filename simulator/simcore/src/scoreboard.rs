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

use tir::backend::liveness::PhysReg;
use tir::backend::regalloc::RegisterInfo;
use tir::backend::sched::{InstrSchedClass, MachineModel};

use crate::MemAccess;
use crate::memsys::MemorySystem;
use crate::predictor::BranchPredictor;

/// One instruction as the engine sees it: its scheduling class, the physical
/// registers it reads/writes, and (in trace mode) its resolved branch outcome.
#[derive(Clone)]
pub struct ScoreboardInstr {
    /// Rendered text for report views; empty when no report is produced.
    pub text: String,
    /// The instruction's op name, for macro-fusion matching. Empty when the
    /// producer has no name (synthetic instructions); such instructions never
    /// fuse.
    pub op_name: String,
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
    /// Encoded instruction length consumed by the fetch frontend.
    pub width_bytes: u16,
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

/// Physical register references as `(class, index)` keys — the granularity the
/// dependence reconstruction works at. `alias` normalizes the class to its
/// physical register file, so classes that alias the same file index-for-index
/// (e.g. arm64 `GPRsp` vs `GPR`) produce matching keys — without it a load's
/// `GPRsp`-classed base address never depends on the `GPR` write that produced
/// it.
pub fn phys_regs(refs: &[PhysReg], alias: Option<&Prf>) -> Vec<(String, u16)> {
    refs.iter()
        .map(|(class, index)| {
            let class = match alias {
                Some(p) => p.file_of(class.name()).to_string(),
                None => class.name().to_string(),
            };
            (class, *index)
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
    /// Byte distance between successive static copies of the analyzed block.
    /// Zero keeps every iteration at the same addresses, as trace replay does.
    pub unroll_stride: u64,
}

#[derive(Default)]
struct FrontendState {
    fetch_cycles: HashMap<u64, FetchCycle>,
    decode_cycles: HashMap<u64, DecodeCycle>,
    decoded_lines: HashMap<usize, Vec<DecodedLine>>,
    decoded_delivery: HashMap<u64, u16>,
    cache_clock: u64,
}

struct FetchCycle {
    window_start: u64,
    bytes: u16,
}

struct DecodeCycle {
    slots: Vec<bool>,
    uops: u16,
}

struct DecodedLine {
    tag: u64,
    instructions: HashSet<u64>,
    uops: u16,
    last_used: u64,
}

impl FrontendState {
    fn reserve_decoded_cache(
        &mut self,
        cache: &tir::backend::sched::DecodedCache,
        class: &InstrSchedClass,
        pc: u64,
        width_bytes: u16,
        earliest: u64,
    ) -> Option<u64> {
        let (set, tag) = decoded_cache_location(cache, pc, width_bytes)?;
        let line = self
            .decoded_lines
            .get_mut(&set)?
            .iter_mut()
            .find(|line| line.tag == tag && line.instructions.contains(&pc))?;
        self.cache_clock += 1;
        line.last_used = self.cache_clock;

        let mut cycle = earliest;
        loop {
            let delivered = self.decoded_delivery.entry(cycle).or_default();
            if delivered.saturating_add(class.decode_uops) <= cache.deliver_uops_per_cycle {
                *delivered = delivered.saturating_add(class.decode_uops);
                return Some(cycle);
            }
            cycle += 1;
        }
    }

    fn fill_decoded_cache(
        &mut self,
        cache: &tir::backend::sched::DecodedCache,
        class: &InstrSchedClass,
        pc: u64,
        width_bytes: u16,
    ) {
        if class.decode_uops > cache.line_uops {
            return;
        }
        let Some((set, tag)) = decoded_cache_location(cache, pc, width_bytes) else {
            return;
        };
        self.cache_clock += 1;
        let lines = self.decoded_lines.entry(set).or_default();
        if let Some(line) = lines.iter_mut().find(|line| line.tag == tag) {
            if line.instructions.contains(&pc)
                || line.uops.saturating_add(class.decode_uops) <= cache.line_uops
            {
                if line.instructions.insert(pc) {
                    line.uops = line.uops.saturating_add(class.decode_uops);
                }
                line.last_used = self.cache_clock;
            }
            return;
        }

        if lines.len() >= usize::from(cache.ways.max(1)) {
            let victim = lines
                .iter()
                .enumerate()
                .min_by_key(|(_, line)| line.last_used)
                .map(|(index, _)| index)
                .unwrap();
            lines.swap_remove(victim);
        }
        lines.push(DecodedLine {
            tag,
            instructions: HashSet::from([pc]),
            uops: class.decode_uops,
            last_used: self.cache_clock,
        });
    }

    fn reserve_fetch(
        &mut self,
        frontend: &tir::backend::sched::Frontend,
        pc: u64,
        width_bytes: u16,
        earliest: u64,
    ) -> u64 {
        let alignment = u64::from(frontend.fetch.alignment.max(1));
        let window_bytes = u64::from(frontend.fetch.window_bytes.max(1));
        let bytes_per_cycle = frontend.fetch.bytes_per_cycle.max(1);
        let mut cursor = pc;
        let mut remaining = width_bytes.max(1);
        let mut cycle = earliest;

        while remaining > 0 {
            let default_window = cursor / alignment * alignment;
            let state = self.fetch_cycles.entry(cycle).or_insert(FetchCycle {
                window_start: default_window,
                bytes: 0,
            });
            let window_end = state.window_start.saturating_add(window_bytes);
            if cursor < state.window_start || cursor >= window_end || state.bytes >= bytes_per_cycle
            {
                cycle += 1;
                continue;
            }
            let in_window = window_end.saturating_sub(cursor).min(u64::from(u16::MAX)) as u16;
            let available = bytes_per_cycle.saturating_sub(state.bytes).min(in_window);
            let consumed = remaining.min(available);
            state.bytes = state.bytes.saturating_add(consumed);
            cursor = cursor.saturating_add(u64::from(consumed));
            remaining -= consumed;
            if remaining > 0 {
                cycle += 1;
            }
        }
        cycle
    }

    fn reserve_decode(
        &mut self,
        frontend: &tir::backend::sched::Frontend,
        class: &InstrSchedClass,
        earliest: u64,
    ) -> u64 {
        let eligible_slots: Vec<_> = frontend
            .decode
            .slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| {
                let decoder = frontend
                    .decode
                    .decoders
                    .iter()
                    .find(|decoder| decoder.name == *slot)?;
                (class.decoder.is_none_or(|required| required == *slot)
                    && decoder.max_uops_per_instruction >= class.decode_uops)
                    .then_some(index)
            })
            .collect();
        if eligible_slots.is_empty() {
            return earliest;
        }

        let mut cycle = earliest;
        loop {
            let uops = self
                .decode_cycles
                .get(&cycle)
                .map(|state| state.uops)
                .unwrap_or(0);
            let occupancy = u64::from(class.decode_cycles.max(1));
            let selected = (uops.saturating_add(class.decode_uops)
                <= frontend.decode.uops_per_cycle)
                .then(|| {
                    eligible_slots.iter().copied().find(|index| {
                        (cycle..cycle + occupancy).all(|occupied_cycle| {
                            self.decode_cycles
                                .get(&occupied_cycle)
                                .is_none_or(|state| !state.slots[*index])
                        })
                    })
                })
                .flatten();
            if let Some(index) = selected {
                for occupied_cycle in cycle..cycle + occupancy {
                    let state =
                        self.decode_cycles
                            .entry(occupied_cycle)
                            .or_insert_with(|| DecodeCycle {
                                slots: vec![false; frontend.decode.slots.len()],
                                uops: 0,
                            });
                    state.slots[index] = true;
                    if occupied_cycle == cycle {
                        state.uops = state.uops.saturating_add(class.decode_uops);
                    }
                }
                return cycle;
            }
            cycle += 1;
        }
    }
}

fn decoded_cache_location(
    cache: &tir::backend::sched::DecodedCache,
    pc: u64,
    width_bytes: u16,
) -> Option<(usize, u64)> {
    let line_bytes = u64::from(cache.line_bytes.max(1));
    let offset = pc % line_bytes;
    if offset + u64::from(width_bytes.max(1)) > line_bytes {
        return None;
    }
    let line = pc / line_bytes;
    let sets = u64::from(cache.sets.max(1));
    Some(((line % sets) as usize, line / sets))
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
                unroll_stride: 0,
            },
            None => Self {
                in_order: true,
                window: 0,
                mispredict_penalty: penalty,
                unroll_stride: 0,
            },
        }
    }

    /// Analyze iterations as contiguous static copies instead of one hot loop.
    pub fn with_unroll_stride(mut self, bytes: u64) -> Self {
        self.unroll_stride = bytes;
        self
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

        // Architectural register count per file: the distinct encoding indices
        // the file's classes name. A class with no encodable register — a status
        // flag, the program counter — names none, and a file that is only such
        // classes gets no entry: nothing renames it, so it gates nothing.
        let mut indices: HashMap<&str, HashSet<u16>> = HashMap::new();
        for class in info.classes {
            if class.registers.is_empty() {
                continue;
            }
            indices
                .entry(class.file)
                .or_default()
                .extend(class.registers.iter().copied());
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
    /// Instruction `i` reserved `resource` for `cycles` at `cycle`. Emitted per
    /// reserved unit, after route selection, so views report the routes the
    /// engine actually took rather than re-deriving them from class metadata.
    fn reserved(&mut self, _cycle: u64, _i: usize, _resource: &'static str, _cycles: u16) {}
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

/// Whether this instance is a dependency-breaking idiom: its class marks it
/// eligible and its data sources are all among its destinations (`xor eax,
/// eax`). Such an instance produces a constant, so the rename stage can satisfy
/// it without reading its sources.
fn is_zero_idiom(slot: &ScoreboardInstr) -> bool {
    slot.class.zero_idiom
        && !slot.uses.is_empty()
        && slot.uses.iter().all(|use_| slot.defs.contains(use_))
}

/// Whether this instance completes in the rename stage: it reserves no
/// execution resource and its result is available the cycle it issues.
fn renamed(slot: &ScoreboardInstr) -> bool {
    slot.class.eliminated || is_zero_idiom(slot)
}

/// The latency of this instance: zero when the rename stage completes it.
fn instr_latency(slot: &ScoreboardInstr) -> u64 {
    if renamed(slot) {
        0
    } else {
        u64::from(slot.class.latency)
    }
}

/// Merge every adjacent macro-fused pair into one instruction: it decodes
/// once, occupies one window slot, executes on the second instruction's units,
/// unions the pair's register accesses, spans both encodings for fetch, and
/// keeps the second's branch outcome.
fn fuse_macro_ops(model: &MachineModel, base: &[ScoreboardInstr]) -> Vec<ScoreboardInstr> {
    let fusable = |first: &ScoreboardInstr, second: &ScoreboardInstr| {
        !first.op_name.is_empty()
            && model.fusions.iter().any(|group| {
                group.first.contains(&first.op_name.as_str())
                    && group.second.contains(&second.op_name.as_str())
            })
    };
    let mut fused = Vec::with_capacity(base.len());
    let mut i = 0;
    while i < base.len() {
        if i + 1 < base.len() && fusable(&base[i], &base[i + 1]) {
            fused.push(fuse_pair(&base[i], &base[i + 1]));
            i += 2;
        } else {
            fused.push(base[i].clone());
            i += 1;
        }
    }
    fused
}

fn fuse_pair(first: &ScoreboardInstr, second: &ScoreboardInstr) -> ScoreboardInstr {
    let mut defs = first.defs.clone();
    defs.extend(second.defs.iter().cloned());
    let mut uses = first.uses.clone();
    uses.extend(second.uses.iter().cloned());
    let mut mem = first.mem.clone();
    mem.extend(second.mem.iter().cloned());
    // A length-changing-prefix stall on either side still applies to the pair.
    let class = InstrSchedClass {
        latency: first.class.latency.max(second.class.latency),
        read_cycle: second.class.read_cycle,
        rthroughput: second.class.rthroughput,
        resources: second.class.resources,
        uops: second.class.uops,
        decode_uops: 1,
        decoder: first.class.decoder.or(second.class.decoder),
        decode_cycles: first.class.decode_cycles.max(second.class.decode_cycles),
        eliminated: false,
        zero_idiom: false,
    };
    ScoreboardInstr {
        text: match (first.text.is_empty(), second.text.is_empty()) {
            (true, true) => String::new(),
            _ => format!("{}; {}", first.text, second.text),
        },
        op_name: second.op_name.clone(),
        class,
        defs,
        uses,
        branch: second.branch,
        pc: first.pc,
        width_bytes: first.width_bytes.saturating_add(second.width_bytes),
        mem,
    }
}

/// The producer→consumer latency between two dependent instructions, honoring
/// the machine's forwarding network and falling back to the producer's latency.
fn edge_latency(
    model: &MachineModel,
    producer: &ScoreboardInstr,
    consumer: &InstrSchedClass,
) -> u64 {
    if renamed(producer) {
        return 0;
    }
    if let (Some(p), Some(c)) = (producer.class.resources.first(), consumer.resources.first())
        && let Some(f) = model.forward_latency(p, c)
    {
        return u64::from(f);
    }
    u64::from(producer.class.latency)
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
    let base = issue_cycle + instr_latency(slot);
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
    let fused_base;
    let base = if model.fusions.is_empty() {
        base
    } else {
        fused_base = fuse_macro_ops(model, base);
        &fused_base
    };

    if !config.in_order
        && predictor.is_none()
        && mem.is_none()
        && base
            .iter()
            .all(|instruction| instruction.branch.is_none() && instruction.mem.is_empty())
    {
        return run_ooo_compute(model, base, iterations, config, prf, handler);
    }

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
    // Cumulative cycles reserved per resource, for route load balancing.
    let mut usage: HashMap<&'static str, u64> = HashMap::new();
    // Earliest cycle the front end may resume after a misprediction redirect.
    let mut redirect: u64 = 0;
    let mut mispredicts: u64 = 0;
    // The simulated clock. Advanced monotonically; between dispatches it skips
    // forward to the next cycle a gate can release rather than spinning.
    let mut cycle: u64 = 0;
    let mut frontend_state = FrontendState::default();

    for i in 0..n {
        let slot = &base[i % base.len()];
        let pc = unrolled_pc(slot.pc, i, base.len(), config.unroll_stride);

        // Front end: advance the clock to instruction `i`'s dispatch cycle. It
        // dispatches in program order, at most `width` per cycle, and no earlier
        // than the ROB has a free window slot, the front end has recovered from
        // any misprediction redirect, and enough physical registers are free.
        let mut d = cycle;
        if i >= width {
            d = d.max(dispatch[i - width] + 1);
        }
        d = reclaim_rob(&mut rob, window, d);
        d = d.max(redirect);
        if let Some(prf) = prf {
            prf_gate(&mut d, slot, prf, &mut prf_inflight);
        }
        // Front-end instruction fetch: only an L1I miss (a new line that missed)
        // stalls dispatch; a hit is folded into the pipeline depth.
        if let Some(mem) = mem.as_deref_mut() {
            d += mem.fetch_stall(pc, d);
        }
        d = frontend_delivery_cycle(model, &mut frontend_state, slot, pc, d);
        cycle = d;
        dispatch[i] = cycle;
        if let Some(h) = handler.as_mut() {
            h.dispatched(cycle, i);
        }

        // Operands ready: the latest forwarding-aware producer result. A
        // dependency-breaking idiom reads none of its sources.
        let operands_ready =
            operands_ready_cycle(model, base, slot, &issue, &result_extra, &reg_writer);

        let mut t = cycle.max(operands_ready);
        if config.in_order && i > 0 {
            t = t.max(issue[i - 1]);
        }

        let mut chosen = Vec::new();
        if !renamed(slot) {
            t = reserve_class_resources(&slot.class, &mut lanes, t, &mut chosen, &usage);
            for (resource, cycles) in &chosen {
                *usage.entry(resource).or_default() += u64::from(*cycles);
            }
        }
        issue[i] = t;
        if let Some(h) = handler.as_mut() {
            for (resource, cycles) in &chosen {
                h.reserved(t, i, resource, *cycles);
            }
            h.issued(t, i);
        }

        for def in &slot.defs {
            reg_writer.insert(def.clone(), i);
        }

        // Branch scoring: compare the predicted direction to the recorded
        // outcome, and stall the front end on a mispredict until the branch
        // resolves plus the refetch penalty.
        if let (Some(p), Some(br)) = (predictor.as_mut(), &slot.branch) {
            mispredicts += score_branch(
                *p,
                br,
                i,
                issue[i] + instr_latency(slot),
                config.mispredict_penalty,
                &mut redirect,
                &mut handler,
            );
        }

        // In-order retire: completes at its (possibly memory-dependent) result
        // cycle, no earlier than its predecessor retires.
        let complete = completion_cycle(slot, issue[i], mem.as_deref_mut());
        // Only a completion that overran the static latency (a cache miss) holds
        // back dependents beyond forwarding; a hit leaves the fast path intact.
        if complete > issue[i] + instr_latency(slot) {
            result_extra[i] = complete;
        }
        retire[i] = complete.max(if i > 0 { retire[i - 1] } else { 0 });
        if let Some(h) = handler.as_mut() {
            h.retired(retire[i], i);
        }
        rob.push_back(retire[i]);

        if let Some(prf) = prf {
            record_prf_allocation(prf, &slot.defs, retire[i], &mut prf_inflight);
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

/// Advance `d` past the ROB window: reclaim slots retired by `d`, and if the
/// ROB is still full, wait for its oldest in-flight instruction to retire.
fn reclaim_rob(rob: &mut Rob, window: usize, mut d: u64) -> u64 {
    while rob.front().is_some_and(|&r| r <= d) {
        rob.pop_front();
    }
    if rob.len() >= window {
        d = d.max(*rob.front().unwrap());
        while rob.front().is_some_and(|&r| r <= d) {
            rob.pop_front();
        }
    }
    d
}

/// The cycle this instance's operands are ready: the latest forwarding-aware
/// producer result. A dependency-breaking idiom reads none of its sources.
fn operands_ready_cycle(
    model: &MachineModel,
    base: &[ScoreboardInstr],
    slot: &ScoreboardInstr,
    issue: &[u64],
    result_extra: &[u64],
    reg_writer: &HashMap<(String, u16), usize>,
) -> u64 {
    let mut ready = 0u64;
    if is_zero_idiom(slot) {
        return ready;
    }
    for u in &slot.uses {
        if let Some(&j) = reg_writer.get(u) {
            let producer = &base[j % base.len()];
            ready = ready
                .max(issue[j] + edge_latency(model, producer, &slot.class))
                .max(result_extra[j]);
        }
    }
    ready
}

/// Compare the predicted direction to the recorded outcome, stalling the front
/// end past `redirect` on a mispredict. Returns the mispredict count to add.
fn score_branch(
    predictor: &mut dyn BranchPredictor,
    branch: &BranchOutcome,
    index: usize,
    resolved: u64,
    penalty: u64,
    redirect: &mut u64,
    handler: &mut Option<&mut dyn EventHandler>,
) -> u64 {
    let predicted = predictor.predict(branch.pc, branch.target);
    let mut mispredicts = 0;
    if predicted != branch.taken {
        mispredicts = 1;
        *redirect = (*redirect).max(resolved + penalty);
        if let Some(h) = handler.as_mut() {
            h.mispredicted(index, resolved, *redirect);
        }
    }
    predictor.update(branch.pc, branch.target, branch.taken);
    mispredicts
}

fn record_prf_allocation(
    prf: &Prf,
    defs: &[(String, u16)],
    retire: u64,
    inflight: &mut HashMap<String, VecDeque<u64>>,
) {
    for (class, _) in defs {
        let file = prf.file_of(class);
        if prf.capacity.contains_key(file) {
            inflight
                .entry(file.to_string())
                .or_default()
                .push_back(retire);
        }
    }
}

fn run_ooo_compute(
    model: &MachineModel,
    base: &[ScoreboardInstr],
    iterations: usize,
    config: &TimingConfig,
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
    if n == 0 {
        if let Some(h) = handler.as_mut() {
            h.finish(0);
        }
        return TimingResult {
            cycles: 0,
            instructions: 0,
            mispredicts: 0,
        };
    }

    let width = usize::from(model.issue_width.max(1));
    let window = if config.window == 0 {
        usize::MAX
    } else {
        config.window
    };
    let dependencies = build_dependencies(base, n);

    let mut lanes: HashMap<&'static str, Vec<u64>> = model
        .resources
        .iter()
        .map(|resource| (resource.name, vec![0; usize::from(resource.units.max(1))]))
        .collect();
    // Cumulative cycles reserved per resource, for route load balancing.
    let mut usage: HashMap<&'static str, u64> = HashMap::new();
    let mut frontend = FrontendState::default();
    let mut frontend_ready: Vec<Option<u64>> = vec![None; n];
    let mut issued: Vec<Option<u64>> = vec![None; n];
    let mut completed: Vec<Option<u64>> = vec![None; n];
    let mut active: VecDeque<usize> = VecDeque::new();
    let mut prf_used: HashMap<String, u16> = HashMap::new();
    let mut next_dispatch = 0usize;
    let mut retired = 0usize;
    let mut cycle = 0u64;

    while retired < n {
        retired += retire_completed(
            base,
            &mut active,
            &completed,
            cycle,
            prf,
            &mut prf_used,
            &mut handler,
        );

        let mut dispatched_this_cycle = 0usize;
        while next_dispatch < n
            && active.len() < window
            && dispatched_this_cycle < width
            && prf_can_allocate(&base[next_dispatch % base.len()].defs, prf, &prf_used)
        {
            let slot = &base[next_dispatch % base.len()];
            let pc = unrolled_pc(slot.pc, next_dispatch, base.len(), config.unroll_stride);
            let ready = *frontend_ready[next_dispatch].get_or_insert_with(|| {
                frontend_delivery_cycle(model, &mut frontend, slot, pc, cycle)
            });
            if ready > cycle {
                break;
            }
            active.push_back(next_dispatch);
            if let Some(prf) = prf {
                for (file, count) in register_file_counts(&slot.defs, prf) {
                    *prf_used.entry(file).or_default() += count;
                }
            }
            if let Some(h) = handler.as_mut() {
                h.dispatched(cycle, next_dispatch);
            }
            next_dispatch += 1;
            dispatched_this_cycle += 1;
        }

        let mut issued_this_cycle = 0usize;
        for &index in &active {
            if issued_this_cycle >= width || issued[index].is_some() {
                continue;
            }
            let slot = &base[index % base.len()];
            if !operands_ready(model, base, slot, &dependencies[index], &issued, cycle) {
                continue;
            }
            let Some(chosen) = reserve_lanes_at(slot, &mut lanes, &mut usage, cycle) else {
                continue;
            };
            issued[index] = Some(cycle);
            completed[index] = Some(cycle + instr_latency(slot));
            issued_this_cycle += 1;
            if let Some(h) = handler.as_mut() {
                for (resource, cycles) in &chosen {
                    h.reserved(cycle, index, resource, *cycles);
                }
                h.issued(cycle, index);
            }
        }

        cycle += 1;
    }

    let cycles = cycle;
    if let Some(h) = handler.as_mut() {
        h.finish(cycles);
    }
    TimingResult {
        cycles,
        instructions: n as u64,
        mispredicts: 0,
    }
}

/// Program-order producer indices per instruction, over the unrolled trace.
fn build_dependencies(base: &[ScoreboardInstr], n: usize) -> Vec<Vec<usize>> {
    let mut dependencies = vec![Vec::new(); n];
    let mut writers = HashMap::new();
    for i in 0..n {
        let slot = &base[i % base.len()];
        if !is_zero_idiom(slot) {
            for register in &slot.uses {
                if let Some(&producer) = writers.get(register) {
                    dependencies[i].push(producer);
                }
            }
        }
        for register in &slot.defs {
            writers.insert(register.clone(), i);
        }
    }
    dependencies
}

/// Retire the in-order prefix of `active` that has completed by `cycle`,
/// releasing its physical registers. Returns how many retired.
fn retire_completed(
    base: &[ScoreboardInstr],
    active: &mut VecDeque<usize>,
    completed: &[Option<u64>],
    cycle: u64,
    prf: Option<&Prf>,
    prf_used: &mut HashMap<String, u16>,
    handler: &mut Option<&mut dyn EventHandler>,
) -> usize {
    let mut retired = 0;
    while let Some(&index) = active.front() {
        if !completed[index].is_some_and(|complete| complete <= cycle) {
            break;
        }
        active.pop_front();
        retired += 1;
        if let Some(prf) = prf {
            for (file, count) in register_file_counts(&base[index % base.len()].defs, prf) {
                let used = prf_used.entry(file).or_default();
                *used = used.saturating_sub(count);
            }
        }
        if let Some(h) = handler.as_mut() {
            h.retired(cycle, index);
        }
    }
    retired
}

fn operands_ready(
    model: &MachineModel,
    base: &[ScoreboardInstr],
    slot: &ScoreboardInstr,
    dependencies: &[usize],
    issued: &[Option<u64>],
    cycle: u64,
) -> bool {
    dependencies.iter().all(|&producer| {
        issued[producer].is_some_and(|producer_issue| {
            let producer_slot = &base[producer % base.len()];
            producer_issue + edge_latency(model, producer_slot, &slot.class) <= cycle
        })
    })
}

/// Reserve this instance's functional-unit lanes at exactly `cycle`, or `None`
/// when no route is free then; a renamed instance reserves nothing.
fn reserve_lanes_at(
    slot: &ScoreboardInstr,
    lanes: &mut HashMap<&'static str, Vec<u64>>,
    usage: &mut HashMap<&'static str, u64>,
    cycle: u64,
) -> Option<Vec<(&'static str, u16)>> {
    let mut chosen = Vec::new();
    if renamed(slot) {
        return Some(chosen);
    }
    let mut candidate_lanes = lanes.clone();
    if reserve_class_resources(&slot.class, &mut candidate_lanes, cycle, &mut chosen, usage)
        != cycle
    {
        return None;
    }
    *lanes = candidate_lanes;
    for (resource, cycles) in &chosen {
        *usage.entry(resource).or_default() += u64::from(*cycles);
    }
    Some(chosen)
}

fn frontend_delivery_cycle(
    model: &MachineModel,
    state: &mut FrontendState,
    slot: &ScoreboardInstr,
    pc: u64,
    earliest: u64,
) -> u64 {
    let Some(frontend) = &model.frontend else {
        return earliest;
    };
    if let Some(decoded) = frontend.decoded_cache.as_ref().and_then(|cache| {
        state.reserve_decoded_cache(cache, &slot.class, pc, slot.width_bytes, earliest)
    }) {
        return decoded;
    }
    let fetched = state.reserve_fetch(frontend, pc, slot.width_bytes, earliest);
    let decoded = state.reserve_decode(frontend, &slot.class, fetched);
    if let Some(cache) = &frontend.decoded_cache {
        state.fill_decoded_cache(cache, &slot.class, pc, slot.width_bytes);
    }
    decoded
}

fn unrolled_pc(base_pc: u64, index: usize, block_len: usize, stride: u64) -> u64 {
    let iteration = (index / block_len.max(1)) as u64;
    base_pc.saturating_add(iteration.saturating_mul(stride))
}

fn register_file_counts(registers: &[(String, u16)], prf: &Prf) -> HashMap<String, u16> {
    let mut counts = HashMap::new();
    for (class, _) in registers {
        *counts.entry(prf.file_of(class).to_string()).or_default() += 1;
    }
    counts
}

fn prf_can_allocate(
    registers: &[(String, u16)],
    prf: Option<&Prf>,
    used: &HashMap<String, u16>,
) -> bool {
    let Some(prf) = prf else {
        return true;
    };
    register_file_counts(registers, prf)
        .into_iter()
        .all(|(file, needed)| {
            used.get(&file).copied().unwrap_or(0).saturating_add(needed)
                <= prf.capacity.get(&file).copied().unwrap_or(u16::MAX)
        })
}

fn reserve_class_resources(
    class: &InstrSchedClass,
    lanes: &mut HashMap<&'static str, Vec<u64>>,
    earliest: u64,
    chosen: &mut Vec<(&'static str, u16)>,
    usage: &HashMap<&'static str, u64>,
) -> u64 {
    if !class.uops.is_empty() {
        return reserve_micro_ops(class.uops, lanes, earliest, chosen, usage);
    }

    let mut issue = earliest;
    for resource in class.resources {
        if let Some(resource_lanes) = lanes.get(*resource) {
            issue = issue.max(resource_lanes.iter().copied().min().unwrap_or(0));
        }
    }
    let busy_until = issue + u64::from(class.rthroughput.max(1));
    for resource in class.resources {
        if let Some(lane) = lanes
            .get_mut(*resource)
            .and_then(|resource_lanes| resource_lanes.iter_mut().min_by_key(|cycle| **cycle))
        {
            *lane = busy_until;
            chosen.push((resource, class.rthroughput.max(1)));
        }
    }
    issue
}

fn reserve_micro_ops(
    uops: &[tir::backend::sched::MicroOp],
    lanes: &mut HashMap<&'static str, Vec<u64>>,
    earliest: u64,
    chosen: &mut Vec<(&'static str, u16)>,
    usage: &HashMap<&'static str, u64>,
) -> u64 {
    let (issue, reserved, mut routes, _) = schedule_micro_ops(uops, lanes.clone(), earliest, usage);
    *lanes = reserved;
    chosen.append(&mut routes);
    issue
}

type ScheduledMicroOps = (
    u64,
    HashMap<&'static str, Vec<u64>>,
    Vec<(&'static str, u16)>,
    u64,
);

fn schedule_micro_ops(
    uops: &[tir::backend::sched::MicroOp],
    lanes: HashMap<&'static str, Vec<u64>>,
    earliest: u64,
    usage: &HashMap<&'static str, u64>,
) -> ScheduledMicroOps {
    let Some((uop, remaining)) = uops.split_first() else {
        return (earliest, lanes, Vec::new(), 0);
    };

    uop.routes
        .iter()
        .map(|route| {
            let (start, reserved) = reserve_route(route, lanes.clone(), earliest);
            let (rest_issue, reserved, mut rest_routes, rest_usage) =
                schedule_micro_ops(remaining, reserved, earliest, usage);
            let mut routes: Vec<(&'static str, u16)> = route
                .resources
                .iter()
                .map(|use_| (use_.resource, use_.cycles.max(1)))
                .collect();
            let route_usage: u64 = routes
                .iter()
                .map(|(r, _)| usage.get(r).copied().unwrap_or(0))
                .sum();
            routes.append(&mut rest_routes);
            (
                start.max(rest_issue),
                reserved,
                routes,
                route_usage + rest_usage,
            )
        })
        // Ties on issue cycle go to the least-used resources so far: stacking
        // onto the unit a steady producer occupies every cycle starves that
        // producer while an equivalent unit idles.
        .min_by_key(|(issue, _, _, route_usage)| (*issue, *route_usage))
        .unwrap_or((earliest, lanes, Vec::new(), 0))
}

fn reserve_route(
    route: &tir::backend::sched::ResourceRoute,
    lanes: HashMap<&'static str, Vec<u64>>,
    earliest: u64,
) -> (u64, HashMap<&'static str, Vec<u64>>) {
    let mut start = earliest;
    loop {
        let mut candidate = lanes.clone();
        let reservable = route.resources.iter().all(|use_| {
            candidate
                .get_mut(use_.resource)
                .and_then(|resource_lanes| {
                    resource_lanes
                        .iter_mut()
                        .filter(|cycle| **cycle <= start)
                        .min_by_key(|cycle| **cycle)
                })
                .map(|lane| *lane = start + u64::from(use_.cycles.max(1)))
                .is_some()
        });
        if reservable {
            return (start, candidate);
        }
        start += 1;
    }
}
