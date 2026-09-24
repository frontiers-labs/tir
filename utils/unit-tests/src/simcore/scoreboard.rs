use std::collections::{HashMap, VecDeque};

use tir::backend::sched::{
    DecodedCache, Decoder, Forward, Frontend, FrontendDecode, FrontendFetch, FusionExpr,
    FusionMemoryRef, FusionMicroOp, FusionOperandSelector, FusionPattern, FusionSchedule,
    FusionStageGroup, FusionStep, InstrSchedClass, MachineModel, MicroOp, ProcUnit, ResourceRoute,
    ResourceUse,
};
use tir_sim::memsys::{CacheParams, MemParams, MemorySystem};
use tir_sim::predictor::{AlwaysNotTaken, BranchPredictor};
use tir_sim::scoreboard::*;
use tir_sim::{MemAccess, MemAccessKind};

/// Verbatim copies of the engine's tiny latency/pressure helpers, so the
/// closed-form oracle below stays a fully independent reimplementation.
fn is_zero_idiom(slot: &ScoreboardInstr) -> bool {
    slot.class.zero_idiom
        && !slot.uses.is_empty()
        && slot.uses.iter().all(|use_| slot.defs.contains(use_))
}

fn renamed(slot: &ScoreboardInstr) -> bool {
    slot.class.eliminated || is_zero_idiom(slot)
}

fn edge_latency(
    model: &MachineModel,
    producer: &ScoreboardInstr,
    consumer: &InstrSchedClass,
) -> u64 {
    if renamed(producer) {
        return 0;
    }
    if let (Some(p), Some(c)) = (producer.class.resources.first(), consumer.resources.first()) {
        if let Some(f) = model.forward_latency(p, c) {
            return u64::from(f);
        }
    }
    u64::from(producer.class.latency)
}

fn file_of<'a>(prf: &'a Prf, class: &'a str) -> &'a str {
    prf.class_to_file
        .get(class)
        .map(String::as_str)
        .unwrap_or(class)
}

fn prf_gate(
    d: &mut u64,
    slot: &ScoreboardInstr,
    prf: &Prf,
    inflight: &mut HashMap<String, VecDeque<u64>>,
) {
    let mut need: HashMap<&str, usize> = HashMap::new();
    for (class, _) in &slot.defs {
        *need.entry(file_of(prf, class)).or_default() += 1;
    }
    for (file, need) in need {
        let Some(&cap) = prf.capacity.get(file) else {
            continue;
        };
        let cap = cap as usize;
        let q = inflight.entry(file.to_string()).or_default();
        while q.front().is_some_and(|&c| c <= *d) {
            q.pop_front();
        }
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

fn reference_operands_ready(
    model: &MachineModel,
    base: &[ScoreboardInstr],
    slot: &ScoreboardInstr,
    issue: &[u64],
    reg_writer: &HashMap<(String, u16), usize>,
) -> u64 {
    let mut operands_ready = 0u64;
    for u in &slot.uses {
        if let Some(&j) = reg_writer.get(u) {
            let producer = &base[j % base.len()];
            operands_ready =
                operands_ready.max(issue[j] + edge_latency(model, producer, &slot.class));
        }
    }
    operands_ready
}

fn earliest_lane_cycle(slot: &ScoreboardInstr, lanes: &HashMap<&str, Vec<u64>>, mut t: u64) -> u64 {
    for r in slot.class.resources {
        if let Some(lane_set) = lanes.get(*r) {
            t = t.max(lane_set.iter().copied().min().unwrap_or(0));
        }
    }
    t
}

fn reserve_lanes(slot: &ScoreboardInstr, lanes: &mut HashMap<&str, Vec<u64>>, busy_until: u64) {
    for r in slot.class.resources {
        if let Some(lane) = lanes
            .get_mut(*r)
            .and_then(|s| s.iter_mut().min_by_key(|c| **c))
        {
            *lane = busy_until;
        }
    }
}

fn reference_score_branch(
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
        let operands_ready = reference_operands_ready(model, base, slot, &issue, &reg_writer);
        let mut t = d.max(operands_ready);
        if config.in_order && i > 0 {
            t = t.max(issue[i - 1]);
        }
        t = earliest_lane_cycle(slot, &lanes, t);
        issue[i] = t;
        if let Some(h) = handler.as_mut() {
            h.issued(t, i);
        }
        reserve_lanes(
            slot,
            &mut lanes,
            t + u64::from(slot.class.rthroughput.max(1)),
        );
        for def in &slot.defs {
            reg_writer.insert(def.clone(), i);
        }
        if let (Some(p), Some(br)) = (predictor.as_mut(), &slot.branch) {
            mispredicts += reference_score_branch(
                *p,
                br,
                i,
                issue[i] + u64::from(slot.class.latency),
                config.mispredict_penalty,
                &mut redirect,
                &mut handler,
            );
        }
        let complete = issue[i] + u64::from(slot.class.latency);
        retire[i] = complete.max(if i > 0 { retire[i - 1] } else { 0 });
        if let Some(h) = handler.as_mut() {
            h.retired(retire[i], i);
        }
        if let Some(prf) = prf {
            for (class, _) in &slot.defs {
                let file = file_of(prf, class);
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
        id: usize::MAX,
        issue_width,
        frontend: None,
        resources,
        buffers: &[],
        pipeline: &[],
        forwards: &[Forward {
            from: "ALU",
            to: "ALU",
            latency: 1,
        }],
        reg_files: &[],
        fusions: &[],
    }
}

const CLASSES: &[InstrSchedClass] = &[
    InstrSchedClass::DEFAULT,
    InstrSchedClass {
        latency: 1,
        read_cycle: 0,
        rthroughput: 1,
        resources: &["ALU"],
        uops: &[],
        decode_uops: 1,
        decoder: None,
        decode_cycles: 1,
        eliminated: false,
        zero_idiom: false,
    },
    InstrSchedClass {
        latency: 3,
        read_cycle: 0,
        rthroughput: 2,
        resources: &["MUL"],
        uops: &[],
        decode_uops: 1,
        decoder: None,
        decode_cycles: 1,
        eliminated: false,
        zero_idiom: false,
    },
    InstrSchedClass {
        latency: 4,
        read_cycle: 0,
        rthroughput: 1,
        resources: &["LSU"],
        uops: &[],
        decode_uops: 1,
        decoder: None,
        decode_cycles: 1,
        eliminated: false,
        zero_idiom: false,
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
                op_name: String::new(),
                class,
                defs,
                uses,
                or_updates: vec![],
                branch,
                pc: 0,
                width_bytes: 1,
                mem: Vec::new(),
                encoded_bytes: None,
                fusion_operands: vec![],
                layout_known: false,
                fusion_boundary: false,
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
                            unroll_stride: 0,
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

const TEST_P0: ResourceUse = ResourceUse {
    resource: "P0",
    cycles: 1,
};
const TEST_P1: ResourceUse = ResourceUse {
    resource: "P1",
    cycles: 1,
};

fn resource_test_model(resources: &'static [ProcUnit]) -> MachineModel {
    MachineModel {
        name: "resource-test",
        id: usize::MAX,
        issue_width: 2,
        frontend: None,
        resources,
        buffers: &[],
        pipeline: &[],
        forwards: &[],
        reg_files: &[],
        fusions: &[],
    }
}

fn resource_test_instr(class: InstrSchedClass) -> ScoreboardInstr {
    ScoreboardInstr {
        text: String::new(),
        op_name: String::new(),
        class,
        defs: vec![],
        uses: vec![],
        or_updates: vec![],
        branch: None,
        pc: 0,
        width_bytes: 1,
        mem: vec![],
        encoded_bytes: None,
        fusion_operands: vec![],
        layout_known: false,
        fusion_boundary: false,
    }
}

fn issue_cycles(model: &MachineModel, program: &[ScoreboardInstr]) -> Vec<u64> {
    issue_cycles_with_order(model, program, false)
}

fn indexed_issue_cycles(model: &MachineModel, program: &[ScoreboardInstr]) -> Vec<u64> {
    let mut events = Recorder::default();
    run(
        model,
        program,
        1,
        &TimingConfig {
            in_order: false,
            window: 0,
            mispredict_penalty: 0,
            unroll_stride: 0,
        },
        None,
        None,
        None,
        Some(&mut events),
    );
    let mut cycles = vec![0; program.len()];
    for (event, cycle, index) in events.0 {
        if event == 'I' {
            cycles[index as usize] = cycle;
        }
    }
    cycles
}

fn issue_cycles_with_order(
    model: &MachineModel,
    program: &[ScoreboardInstr],
    in_order: bool,
) -> Vec<u64> {
    let mut events = Recorder::default();
    run(
        model,
        program,
        1,
        &TimingConfig {
            in_order,
            window: 0,
            mispredict_penalty: 0,
            unroll_stride: 0,
        },
        None,
        None,
        None,
        Some(&mut events),
    );
    events
        .0
        .iter()
        .filter_map(|(event, cycle, _)| (*event == 'I').then_some(*cycle))
        .collect()
}

fn dependency_test_instr(
    latency: u16,
    defs: &[(&str, u16)],
    uses: &[(&str, u16)],
    or_updates: &[(&str, u16)],
) -> ScoreboardInstr {
    let registers = |registers: &[(&str, u16)]| {
        registers
            .iter()
            .map(|(class, index)| ((*class).to_string(), *index))
            .collect()
    };
    let mut instruction = resource_test_instr(InstrSchedClass {
        latency,
        ..InstrSchedClass::DEFAULT
    });
    instruction.defs = registers(defs);
    instruction.uses = registers(uses);
    instruction.or_updates = registers(or_updates);
    instruction
}

#[test]
fn or_updates_do_not_serialize_and_readers_wait_for_every_contributor() {
    let model = resource_test_model(&[]);
    let flags = [("CSR", 1)];
    let program = [
        dependency_test_instr(9, &flags, &flags, &flags),
        dependency_test_instr(2, &flags, &flags, &flags),
        dependency_test_instr(1, &[], &flags, &[]),
    ];

    assert_eq!(issue_cycles(&model, &program), vec![0, 0, 9]);
}

#[test]
fn readers_wait_for_an_older_contributor_that_issues_later() {
    let model = resource_test_model(&[]);
    let flags = [("CSR", 1)];
    let gate = [("GPR", 0)];
    let program = [
        dependency_test_instr(7, &gate, &[], &[]),
        dependency_test_instr(2, &flags, &[flags[0], gate[0]], &flags),
        dependency_test_instr(4, &flags, &flags, &flags),
        dependency_test_instr(1, &[], &flags, &[]),
        dependency_test_instr(1, &[], &flags, &[]),
        dependency_test_instr(1, &flags, &[], &[]),
        dependency_test_instr(1, &[], &flags, &[]),
    ];

    assert_eq!(
        indexed_issue_cycles(&model, &program),
        vec![0, 7, 1, 9, 9, 2, 3]
    );
}

#[test]
fn contributor_readiness_respects_each_consumers_forwarding_path() {
    const RESOURCES: &[ProcUnit] = &[
        ProcUnit {
            name: "P0",
            units: 2,
        },
        ProcUnit {
            name: "P1",
            units: 2,
        },
    ];
    let mut model = resource_test_model(RESOURCES);
    model.forwards = &[Forward {
        from: "P0",
        to: "P1",
        latency: 2,
    }];
    let flags = [("CSR", 1)];
    let class = |latency, resource| InstrSchedClass {
        latency,
        resources: resource,
        ..InstrSchedClass::DEFAULT
    };
    let mut slow_forwarded = dependency_test_instr(9, &flags, &flags, &flags);
    slow_forwarded.class = class(9, &["P0"]);
    let mut ordinary = dependency_test_instr(4, &flags, &flags, &flags);
    ordinary.class = class(4, &["P1"]);
    let mut forwarded_reader = dependency_test_instr(1, &[], &flags, &[]);
    forwarded_reader.class = class(1, &["P1"]);
    let mut unforwarded_reader = dependency_test_instr(1, &[], &flags, &[]);
    unforwarded_reader.class = class(1, &["P0"]);

    assert_eq!(
        issue_cycles(
            &model,
            &[
                slow_forwarded,
                ordinary,
                forwarded_reader,
                unforwarded_reader,
            ],
        ),
        vec![0, 0, 4, 9]
    );
}

#[test]
fn general_out_of_order_path_tracks_every_or_update() {
    let model = resource_test_model(&[]);
    let flags = [("CSR", 1)];
    let mut long = dependency_test_instr(9, &flags, &flags, &flags);
    long.branch = Some(BranchOutcome {
        pc: 0,
        target: 4,
        taken: false,
    });
    let program = [
        long,
        dependency_test_instr(2, &flags, &flags, &flags),
        dependency_test_instr(1, &[], &flags, &[]),
    ];

    assert_eq!(issue_cycles(&model, &program), vec![0, 0, 9]);
}

#[test]
fn memory_enabled_timing_tracks_every_or_update() {
    let model = resource_test_model(&[]);
    let flags = [("CSR", 1)];
    let program = [
        dependency_test_instr(9, &flags, &flags, &flags),
        dependency_test_instr(2, &flags, &flags, &flags),
        dependency_test_instr(1, &[], &flags, &[]),
    ];
    let cache = CacheParams {
        size: 64,
        ways: 1,
        line: 64,
        latency: 1,
        banks: 1,
        mshrs: 1,
    };
    let mut memory = MemorySystem::new(MemParams {
        l1i: cache,
        l1d: cache,
        l2: None,
        l3: None,
        dram_latency: 1,
        dram_streams: 1,
    });
    let mut events = Recorder::default();
    run(
        &model,
        &program,
        1,
        &TimingConfig {
            in_order: false,
            window: 0,
            mispredict_penalty: 0,
            unroll_stride: 0,
        },
        None,
        None,
        Some(&mut memory),
        Some(&mut events),
    );
    let issues: Vec<_> = events
        .0
        .iter()
        .filter_map(|(event, cycle, _)| (*event == 'I').then_some(*cycle))
        .collect();

    assert_eq!(issues[1], issues[0]);
    assert_eq!(issues[2], issues[0] + 9);
}

#[test]
fn in_order_issue_keeps_or_updates_conservative() {
    let model = resource_test_model(&[]);
    let flags = [("CSR", 1)];
    let program = [
        dependency_test_instr(9, &flags, &flags, &flags),
        dependency_test_instr(2, &flags, &flags, &flags),
        dependency_test_instr(1, &[], &flags, &[]),
    ];

    assert_eq!(
        issue_cycles_with_order(&model, &program, true),
        vec![0, 9, 11]
    );
}

#[test]
fn flag_overwrite_clears_the_contributor_frontier() {
    let model = resource_test_model(&[]);
    let flags = [("CSR", 1)];
    let program = [
        dependency_test_instr(9, &flags, &flags, &flags),
        dependency_test_instr(2, &flags, &[], &[]),
        dependency_test_instr(1, &[], &flags, &[]),
    ];

    assert_eq!(issue_cycles(&model, &program), vec![0, 0, 2]);
}

#[test]
fn ordinary_flag_rmw_consumes_then_replaces_the_frontier() {
    let model = resource_test_model(&[]);
    let flags = [("CSR", 1)];
    let program = [
        dependency_test_instr(9, &flags, &flags, &flags),
        dependency_test_instr(2, &flags, &flags, &[]),
        dependency_test_instr(1, &[], &flags, &[]),
    ];

    assert_eq!(issue_cycles(&model, &program), vec![0, 9, 11]);
}

#[test]
fn or_updates_preserve_ordinary_register_dependencies() {
    let model = resource_test_model(&[]);
    let flags = [("CSR", 1)];
    let value = [("FPR", 3)];
    let program = [
        dependency_test_instr(7, &value, &[], &[]),
        dependency_test_instr(2, &flags, &[flags[0], value[0]], &flags),
    ];

    assert_eq!(issue_cycles(&model, &program), vec![0, 7]);
}

#[test]
fn normalized_flag_aliases_share_one_contributor_frontier() {
    let model = resource_test_model(&[]);
    let normalized_flags = [("CSR", 1)];
    let program = [
        dependency_test_instr(8, &normalized_flags, &normalized_flags, &normalized_flags),
        dependency_test_instr(1, &[], &normalized_flags, &[]),
    ];

    assert_eq!(issue_cycles(&model, &program), vec![0, 8]);
}

fn pair_rule(first: &'static str, second: &'static str) -> &'static [FusionPattern] {
    const INPUTS: &[FusionOperandSelector] = &[
        FusionOperandSelector::AllInputs(0),
        FusionOperandSelector::AllInputs(1),
    ];
    const OUTPUTS: &[FusionOperandSelector] = &[
        FusionOperandSelector::AllOutputs(0),
        FusionOperandSelector::AllOutputs(1),
    ];
    const MEMORY: &[FusionMemoryRef] = &[
        FusionMemoryRef {
            step: 0,
            index: None,
        },
        FusionMemoryRef {
            step: 1,
            index: None,
        },
    ];
    const UOPS: &[FusionMicroOp] = &[FusionMicroOp {
        name: "pair",
        routes: &[],
        inherit_routes: Some(1),
        inputs: INPUTS,
        outputs: OUTPUTS,
        depends_on: &[],
        read_cycle: 0,
        write_cycle: 1,
        memory: MEMORY,
        control_steps: &[0, 1],
    }];
    let steps = Box::leak(Box::new([
        FusionStep {
            ops: Box::leak(Box::new([first])),
        },
        FusionStep {
            ops: Box::leak(Box::new([second])),
        },
    ]));
    Box::leak(Box::new([FusionPattern {
        name: "pair",
        steps,
        guard: FusionExpr::True,
        schedule: FusionSchedule {
            decode_uops: 1,
            decoded_cache_uops: 1,
            decoder: None,
            decode_cycles: 1,
            rename_slots: 1,
            rob_slots: 1,
            retire_slots: 1,
            decode_groups: &[],
            rename_groups: &[],
            rob_groups: &[],
            retire_groups: &[],
            uops: UOPS,
        },
    }]))
}

#[test]
fn fusion_drops_or_update_when_the_other_instruction_reads_the_flags() {
    let mut model = resource_test_model(&[]);
    model.fusions = pair_rule("update", "read");
    let flags = [("CSR", 1)];
    let mut contributor = dependency_test_instr(9, &flags, &flags, &flags);
    contributor.op_name = "contributor".to_string();
    let mut update = dependency_test_instr(1, &flags, &flags, &flags);
    update.op_name = "update".to_string();
    let mut read = dependency_test_instr(1, &[], &flags, &[]);
    read.op_name = "read".to_string();

    assert_eq!(
        issue_cycles(&model, &[contributor, update, read]),
        vec![0, 9, 9]
    );
}

#[test]
fn fusion_preserves_or_update_when_both_instructions_are_contributors() {
    let mut model = resource_test_model(&[]);
    model.fusions = pair_rule("first-update", "second-update");
    let flags = [("CSR", 1)];
    let mut long = dependency_test_instr(9, &flags, &flags, &flags);
    long.op_name = "long".to_string();
    let mut first = dependency_test_instr(1, &flags, &flags, &flags);
    first.op_name = "first-update".to_string();
    let mut second = dependency_test_instr(1, &flags, &flags, &flags);
    second.op_name = "second-update".to_string();
    let read = dependency_test_instr(1, &[], &flags, &[]);

    assert_eq!(
        issue_cycles(&model, &[long, first, second, read]),
        vec![0, 0, 0, 9]
    );
}

const TEST_DECODERS: &[Decoder] = &[
    Decoder {
        name: "simple",
        max_uops_per_instruction: 1,
    },
    Decoder {
        name: "complex",
        max_uops_per_instruction: 4,
    },
];

fn frontend_test_model(slots: &'static [&'static str], uops_per_cycle: u16) -> MachineModel {
    let mut model = resource_test_model(&[]);
    model.issue_width = 4;
    model.frontend = Some(Frontend {
        fetch: FrontendFetch {
            bytes_per_cycle: 16,
            window_bytes: 16,
            alignment: 16,
            queue_bytes: 64,
        },
        decode: FrontendDecode {
            slots,
            uops_per_cycle,
            queue_uops: 32,
            decoders: TEST_DECODERS,
        },
        decoded_cache: None,
    });
    model
}

/// A fused pair (`cmp` + `jne` by op name) decodes and executes as
/// one micro-op: on a single-unit, single-issue core the pair sustains one
/// iteration per cycle where the unfused pair needs two.
#[test]
fn macro_fused_pair_costs_one_micro_op() {
    const ROUTE_P0: ResourceRoute = ResourceRoute {
        resources: &[TEST_P0],
    };
    const UOP_P0: MicroOp = MicroOp {
        routes: &[ROUTE_P0],
    };

    let mut model = resource_test_model(&[ProcUnit {
        name: "P0",
        units: 1,
    }]);
    model.issue_width = 1;
    model.fusions = pair_rule("cmp", "jne");

    let class = InstrSchedClass {
        uops: &[UOP_P0],
        resources: &[],
        ..InstrSchedClass::DEFAULT
    };
    let mut cmp = resource_test_instr(class);
    cmp.op_name = "cmp".to_string();
    let mut jne = resource_test_instr(class);
    jne.op_name = "jne".to_string();

    let result = run(
        &model,
        &[cmp, jne],
        64,
        &TimingConfig {
            in_order: false,
            window: 0,
            mispredict_penalty: 0,
            unroll_stride: 0,
        },
        None,
        None,
        None,
        None,
    );
    assert!(
        result.cycles <= 66,
        "one fused micro-op per iteration expected, got {} cycles for 64 iterations",
        result.cycles
    );
}

/// A producer that owns P0 every cycle feeds a consumer that may use P0 or
/// P1. The consumer must settle on the idle P1, sustaining one iteration
/// per cycle; packing it onto P0 (which route tie-breaking once preferred)
/// halves throughput.
#[test]
fn micro_op_avoids_a_saturated_resource_when_an_idle_one_exists() {
    const ROUTE_P0: ResourceRoute = ResourceRoute {
        resources: &[TEST_P0],
    };
    const ROUTE_P1: ResourceRoute = ResourceRoute {
        resources: &[TEST_P1],
    };
    const FIXED: MicroOp = MicroOp {
        routes: &[ROUTE_P0],
    };
    const FLEXIBLE: MicroOp = MicroOp {
        routes: &[ROUTE_P0, ROUTE_P1],
    };

    let model = resource_test_model(&[
        ProcUnit {
            name: "P0",
            units: 1,
        },
        ProcUnit {
            name: "P1",
            units: 1,
        },
    ]);
    let mut producer = resource_test_instr(InstrSchedClass {
        uops: &[FIXED],
        resources: &[],
        ..InstrSchedClass::DEFAULT
    });
    producer.defs = vec![("R".to_string(), 0)];
    let mut consumer = resource_test_instr(InstrSchedClass {
        uops: &[FLEXIBLE],
        resources: &[],
        ..InstrSchedClass::DEFAULT
    });
    consumer.uses = vec![("R".to_string(), 0)];

    let result = run(
        &model,
        &[producer, consumer],
        64,
        &TimingConfig {
            in_order: false,
            window: 0,
            mispredict_penalty: 0,
            unroll_stride: 0,
        },
        None,
        None,
        None,
        None,
    );
    assert!(
        result.cycles <= 66,
        "one iteration per cycle expected, got {} cycles for 64 iterations",
        result.cycles
    );
}

#[test]
fn micro_op_chooses_an_available_alternative_resource() {
    const ROUTE_P0: ResourceRoute = ResourceRoute {
        resources: &[TEST_P0],
    };
    const ROUTE_P1: ResourceRoute = ResourceRoute {
        resources: &[TEST_P1],
    };
    const ALTERNATIVE: MicroOp = MicroOp {
        routes: &[ROUTE_P0, ROUTE_P1],
    };

    let model = resource_test_model(&[
        ProcUnit {
            name: "P0",
            units: 1,
        },
        ProcUnit {
            name: "P1",
            units: 1,
        },
    ]);
    let program = [
        resource_test_instr(InstrSchedClass {
            latency: 1,
            read_cycle: 0,
            rthroughput: 1,
            resources: &["P0"],
            uops: &[],
            decode_uops: 1,
            decoder: None,
            decode_cycles: 1,
            eliminated: false,
            zero_idiom: false,
        }),
        resource_test_instr(InstrSchedClass {
            latency: 1,
            read_cycle: 0,
            rthroughput: 1,
            resources: &[],
            uops: &[ALTERNATIVE],
            decode_uops: 1,
            decoder: None,
            decode_cycles: 1,
            eliminated: false,
            zero_idiom: false,
        }),
    ];

    assert_eq!(issue_cycles(&model, &program), vec![0, 0]);
}

#[derive(Default)]
struct ReservationRecorder(Vec<(usize, &'static str, u16)>);

impl EventHandler for ReservationRecorder {
    fn reserved(&mut self, _cycle: u64, i: usize, resource: &'static str, cycles: u16) {
        self.0.push((i, resource, cycles));
    }
    fn render(&self) -> String {
        String::new()
    }
}

#[test]
fn reserved_events_report_the_chosen_route() {
    const ROUTE_P0: ResourceRoute = ResourceRoute {
        resources: &[TEST_P0],
    };
    const ROUTE_P1: ResourceRoute = ResourceRoute {
        resources: &[TEST_P1],
    };
    const ALTERNATIVE: MicroOp = MicroOp {
        routes: &[ROUTE_P0, ROUTE_P1],
    };

    let model = resource_test_model(&[
        ProcUnit {
            name: "P0",
            units: 1,
        },
        ProcUnit {
            name: "P1",
            units: 1,
        },
    ]);
    let program = [
        resource_test_instr(InstrSchedClass {
            latency: 1,
            read_cycle: 0,
            rthroughput: 1,
            resources: &["P0"],
            uops: &[],
            decode_uops: 1,
            decoder: None,
            decode_cycles: 1,
            eliminated: false,
            zero_idiom: false,
        }),
        resource_test_instr(InstrSchedClass {
            latency: 1,
            read_cycle: 0,
            rthroughput: 1,
            resources: &[],
            uops: &[ALTERNATIVE],
            decode_uops: 1,
            decoder: None,
            decode_cycles: 1,
            eliminated: false,
            zero_idiom: false,
        }),
    ];

    let mut events = ReservationRecorder::default();
    run(
        &model,
        &program,
        1,
        &TimingConfig {
            in_order: false,
            window: 0,
            mispredict_penalty: 0,
            unroll_stride: 0,
        },
        None,
        None,
        None,
        Some(&mut events),
    );
    // The legacy instruction holds P0, so the alternative-routed micro-op
    // must report the P1 route it actually took.
    assert_eq!(events.0, vec![(0, "P0", 1), (1, "P1", 1)]);
}

#[test]
fn fused_inherited_route_reservation_belongs_to_its_member() {
    const P0_ROUTE: ResourceRoute = ResourceRoute {
        resources: &[TEST_P0],
    };
    const P1_ROUTE: ResourceRoute = ResourceRoute {
        resources: &[TEST_P1],
    };
    const P0_UOP: MicroOp = MicroOp {
        routes: &[P0_ROUTE],
    };
    const P1_UOP: MicroOp = MicroOp {
        routes: &[P1_ROUTE],
    };
    let mut model = resource_test_model(&[
        ProcUnit {
            name: "P0",
            units: 1,
        },
        ProcUnit {
            name: "P1",
            units: 1,
        },
    ]);
    model.fusions = pair_rule("cmp", "jne");
    let mut cmp = resource_test_instr(InstrSchedClass {
        uops: &[P0_UOP],
        ..InstrSchedClass::DEFAULT
    });
    cmp.op_name = "cmp".to_string();
    let mut jne = resource_test_instr(InstrSchedClass {
        uops: &[P1_UOP],
        ..InstrSchedClass::DEFAULT
    });
    jne.op_name = "jne".to_string();
    let mut events = ReservationRecorder::default();
    run(
        &model,
        &[cmp, jne],
        1,
        &TimingConfig {
            in_order: false,
            window: 0,
            mispredict_penalty: 0,
            unroll_stride: 0,
        },
        None,
        None,
        None,
        Some(&mut events),
    );
    assert_eq!(events.0, vec![(1, "P1", 1)]);
}

#[test]
fn resource_occupancy_delays_the_next_micro_op() {
    const P0_THREE_CYCLES: ResourceUse = ResourceUse {
        resource: "P0",
        cycles: 3,
    };
    const ROUTE: ResourceRoute = ResourceRoute {
        resources: &[P0_THREE_CYCLES],
    };
    const UOP: MicroOp = MicroOp { routes: &[ROUTE] };

    let model = resource_test_model(&[ProcUnit {
        name: "P0",
        units: 1,
    }]);
    let program = [
        resource_test_instr(InstrSchedClass {
            latency: 1,
            read_cycle: 0,
            rthroughput: 1,
            resources: &[],
            uops: &[UOP],
            decode_uops: 1,
            decoder: None,
            decode_cycles: 1,
            eliminated: false,
            zero_idiom: false,
        }),
        resource_test_instr(InstrSchedClass {
            latency: 1,
            read_cycle: 0,
            rthroughput: 1,
            resources: &[],
            uops: &[UOP],
            decode_uops: 1,
            decoder: None,
            decode_cycles: 1,
            eliminated: false,
            zero_idiom: false,
        }),
    ];

    assert_eq!(issue_cycles(&model, &program), vec![0, 3]);
}

#[test]
fn conjunctive_route_waits_for_every_resource() {
    const BOTH: ResourceRoute = ResourceRoute {
        resources: &[TEST_P0, TEST_P1],
    };
    const UOP: MicroOp = MicroOp { routes: &[BOTH] };

    let model = resource_test_model(&[
        ProcUnit {
            name: "P0",
            units: 1,
        },
        ProcUnit {
            name: "P1",
            units: 1,
        },
    ]);
    let program = [
        resource_test_instr(InstrSchedClass {
            latency: 1,
            read_cycle: 0,
            rthroughput: 1,
            resources: &["P1"],
            uops: &[],
            decode_uops: 1,
            decoder: None,
            decode_cycles: 1,
            eliminated: false,
            zero_idiom: false,
        }),
        resource_test_instr(InstrSchedClass {
            latency: 1,
            read_cycle: 0,
            rthroughput: 1,
            resources: &[],
            uops: &[UOP],
            decode_uops: 1,
            decoder: None,
            decode_cycles: 1,
            eliminated: false,
            zero_idiom: false,
        }),
    ];

    assert_eq!(issue_cycles(&model, &program), vec![0, 1]);
}

#[test]
fn complex_decoder_slot_accepts_one_instruction_per_cycle() {
    let model = frontend_test_model(&["complex", "simple"], 4);
    let complex = InstrSchedClass {
        latency: 1,
        read_cycle: 0,
        rthroughput: 1,
        resources: &[],
        uops: &[],
        decode_uops: 3,
        decoder: Some("complex"),
        decode_cycles: 1,
        eliminated: false,
        zero_idiom: false,
    };
    let program = [resource_test_instr(complex), resource_test_instr(complex)];

    assert_eq!(issue_cycles(&model, &program), vec![0, 1]);
}

#[test]
fn decoder_occupancy_reserves_its_slot_for_multiple_cycles() {
    let model = frontend_test_model(&["complex"], 4);
    let complex = InstrSchedClass {
        latency: 1,
        read_cycle: 0,
        rthroughput: 1,
        resources: &[],
        uops: &[],
        decode_uops: 1,
        decoder: Some("complex"),
        decode_cycles: 3,
        eliminated: false,
        zero_idiom: false,
    };
    let program = [resource_test_instr(complex), resource_test_instr(complex)];

    assert_eq!(issue_cycles(&model, &program), vec![0, 3]);
}

#[test]
fn decode_uop_bandwidth_is_shared_by_all_slots() {
    let model = frontend_test_model(&["complex", "complex"], 4);
    let class = InstrSchedClass {
        latency: 1,
        read_cycle: 0,
        rthroughput: 1,
        resources: &[],
        uops: &[],
        decode_uops: 3,
        decoder: None,
        decode_cycles: 1,
        eliminated: false,
        zero_idiom: false,
    };
    let program = [resource_test_instr(class), resource_test_instr(class)];

    assert_eq!(issue_cycles(&model, &program), vec![0, 1]);
}

#[test]
fn fetch_bandwidth_limits_instruction_delivery() {
    let mut model = frontend_test_model(&["complex", "simple"], 4);
    model.frontend.as_mut().unwrap().fetch.bytes_per_cycle = 4;
    let mut first = resource_test_instr(InstrSchedClass::DEFAULT);
    first.width_bytes = 4;
    let mut second = resource_test_instr(InstrSchedClass::DEFAULT);
    second.pc = 4;
    second.width_bytes = 4;

    assert_eq!(issue_cycles(&model, &[first, second]), vec![0, 1]);
}

#[test]
fn fetch_window_boundary_splits_an_instruction() {
    let mut model = frontend_test_model(&["complex", "simple"], 4);
    let fetch = &mut model.frontend.as_mut().unwrap().fetch;
    fetch.bytes_per_cycle = 16;
    fetch.window_bytes = 8;
    fetch.alignment = 8;
    let mut instruction = resource_test_instr(InstrSchedClass::DEFAULT);
    instruction.pc = 4;
    instruction.width_bytes = 12;

    assert_eq!(issue_cycles(&model, &[instruction]), vec![1]);
}

#[test]
fn decoded_cache_bypasses_fetch_and_decode_after_warmup() {
    let mut model = frontend_test_model(&["simple"], 1);
    model.issue_width = 1;
    let frontend = model.frontend.as_mut().unwrap();
    frontend.fetch.bytes_per_cycle = 1;
    frontend.decoded_cache = Some(DecodedCache {
        sets: 1,
        ways: 1,
        line_bytes: 16,
        line_uops: 8,
        deliver_uops_per_cycle: 4,
    });
    let mut first = resource_test_instr(InstrSchedClass::DEFAULT);
    first.width_bytes = 4;
    let mut repeated = resource_test_instr(InstrSchedClass::DEFAULT);
    repeated.width_bytes = 4;

    assert_eq!(issue_cycles(&model, &[first, repeated]), vec![3, 4]);
}

#[test]
fn static_unroll_uses_distinct_frontend_addresses() {
    let mut model = frontend_test_model(&["simple"], 1);
    model.frontend.as_mut().unwrap().decoded_cache = Some(DecodedCache {
        sets: 1,
        ways: 1,
        line_bytes: 16,
        line_uops: 8,
        deliver_uops_per_cycle: 4,
    });
    let instruction = resource_test_instr(InstrSchedClass::DEFAULT);
    let mut events = Recorder::default();

    run(
        &model,
        &[instruction],
        2,
        &TimingConfig::for_model(&model).with_unroll_stride(1),
        None,
        None,
        None,
        Some(&mut events),
    );

    let issue_cycles: Vec<_> = events
        .0
        .into_iter()
        .filter_map(|(event, cycle, _)| (event == 'I').then_some(cycle))
        .collect();
    assert_eq!(issue_cycles, vec![0, 1]);
}

#[test]
fn out_of_order_issue_does_not_reserve_for_an_unready_older_instruction() {
    let mut model = resource_test_model(&[ProcUnit {
        name: "MUL",
        units: 1,
    }]);
    model.issue_width = 4;
    let producer = resource_test_instr(InstrSchedClass {
        latency: 5,
        read_cycle: 0,
        rthroughput: 1,
        resources: &[],
        uops: &[],
        decode_uops: 1,
        decoder: None,
        decode_cycles: 1,
        eliminated: false,
        zero_idiom: false,
    });
    let mul = InstrSchedClass {
        latency: 1,
        read_cycle: 0,
        rthroughput: 1,
        resources: &["MUL"],
        uops: &[],
        decode_uops: 1,
        decoder: None,
        decode_cycles: 1,
        eliminated: false,
        zero_idiom: false,
    };
    let mut producer = producer;
    producer.defs = vec![("GPR".to_string(), 0)];
    let mut dependent = resource_test_instr(mul);
    dependent.uses = vec![("GPR".to_string(), 0)];
    let independent = resource_test_instr(mul);
    let program = [producer, dependent, independent];
    let mut events = Recorder::default();
    run(
        &model,
        &program,
        1,
        &TimingConfig {
            in_order: false,
            window: 0,
            mispredict_penalty: 0,
            unroll_stride: 0,
        },
        None,
        None,
        None,
        Some(&mut events),
    );
    let mut issued = vec![0; program.len()];
    for (event, cycle, index) in events.0 {
        if event == 'I' {
            issued[index as usize] = cycle;
        }
    }

    assert_eq!(issued, vec![0, 5, 0]);
}

const RENAME_ALU: InstrSchedClass = InstrSchedClass {
    latency: 1,
    read_cycle: 0,
    rthroughput: 1,
    resources: &["ALU"],
    uops: &[],
    decode_uops: 1,
    decoder: None,
    decode_cycles: 1,
    eliminated: false,
    zero_idiom: false,
};

fn rename_test_model() -> MachineModel {
    let mut model = resource_test_model(&[ProcUnit {
        name: "ALU",
        units: 1,
    }]);
    model.issue_width = 4;
    model
}

/// A recurrent chain `r1 <- r0, r2 <- r1, r3 <- r2, r0 <- r3`.
fn move_chain(class: InstrSchedClass) -> Vec<ScoreboardInstr> {
    (0..4)
        .map(|i| {
            let mut instruction = resource_test_instr(class);
            instruction.uses = vec![("GPR".to_string(), i)];
            instruction.defs = vec![("GPR".to_string(), (i + 1) % 4)];
            instruction
        })
        .collect()
}

fn timing(model: &MachineModel, program: &[ScoreboardInstr], in_order: bool) -> TimingResult {
    run(
        model,
        program,
        64,
        &TimingConfig {
            in_order,
            window: 0,
            mispredict_penalty: 0,
            unroll_stride: 0,
        },
        None,
        None,
        None,
        None,
    )
}

#[test]
fn eliminated_moves_are_issue_width_bound_out_of_order() {
    let model = rename_test_model();
    let baseline = timing(&model, &move_chain(RENAME_ALU), false);
    let eliminated = timing(
        &model,
        &move_chain(InstrSchedClass {
            eliminated: true,
            latency: 0,
            resources: &[],
            ..RENAME_ALU
        }),
        false,
    );

    assert!(baseline.ipc() <= 1.0, "baseline ipc {}", baseline.ipc());
    assert!(
        eliminated.ipc() > 3.0,
        "eliminated ipc {}",
        eliminated.ipc()
    );
}

#[test]
fn eliminated_moves_are_issue_width_bound_in_order() {
    let model = rename_test_model();
    let baseline = timing(&model, &move_chain(RENAME_ALU), true);
    let eliminated = timing(
        &model,
        &move_chain(InstrSchedClass {
            eliminated: true,
            latency: 0,
            resources: &[],
            ..RENAME_ALU
        }),
        true,
    );

    assert!(baseline.ipc() <= 1.0, "baseline ipc {}", baseline.ipc());
    assert!(
        eliminated.ipc() > 3.0,
        "eliminated ipc {}",
        eliminated.ipc()
    );
}

#[test]
fn an_eliminated_move_still_waits_for_its_source() {
    let mut model = rename_test_model();
    model.issue_width = 4;
    let mut producer = resource_test_instr(InstrSchedClass {
        latency: 5,
        resources: &[],
        ..RENAME_ALU
    });
    producer.defs = vec![("GPR".to_string(), 0)];
    let mut mov = resource_test_instr(InstrSchedClass {
        eliminated: true,
        latency: 0,
        resources: &[],
        ..RENAME_ALU
    });
    mov.uses = vec![("GPR".to_string(), 0)];
    mov.defs = vec![("GPR".to_string(), 1)];
    let mut consumer = resource_test_instr(RENAME_ALU);
    consumer.uses = vec![("GPR".to_string(), 1)];
    consumer.defs = vec![("GPR".to_string(), 2)];
    let program = [producer, mov, consumer];

    assert_eq!(issue_cycles(&model, &program), vec![0, 5, 5]);
}

#[test]
fn a_same_register_zero_idiom_breaks_its_dependency() {
    let model = rename_test_model();
    let class = InstrSchedClass {
        zero_idiom: true,
        ..RENAME_ALU
    };
    let mut idiom = resource_test_instr(class);
    idiom.uses = vec![("GPR".to_string(), 0)];
    idiom.defs = vec![("GPR".to_string(), 0)];
    let program: Vec<_> = (0..4).map(|_| idiom_clone(&idiom)).collect();

    let result = timing(&model, &program, false);
    assert!(result.ipc() > 3.0, "zero idiom ipc {}", result.ipc());
}

#[test]
fn a_different_register_zero_idiom_executes_normally() {
    let model = rename_test_model();
    let class = InstrSchedClass {
        zero_idiom: true,
        ..RENAME_ALU
    };
    let result = timing(&model, &move_chain(class), false);

    assert!(result.ipc() <= 1.0, "xor chain ipc {}", result.ipc());
}

#[test]
fn probe_stream_throughput() {
    for (width, wb, al) in [
        (5u16, 16u16, 16u16),
        (5, 32, 16),
        (5, 32, 32),
        (3, 16, 16),
        (3, 32, 16),
        (2, 16, 16),
        (2, 32, 16),
        (4, 32, 16),
        (6, 32, 16),
        (7, 32, 16),
        (10, 32, 16),
    ] {
        let mut model = frontend_test_model(&["simple", "simple", "simple", "simple"], 4);
        model.issue_width = 6;
        {
            let f = &mut model.frontend.as_mut().unwrap().fetch;
            f.window_bytes = wb;
            f.alignment = al;
        }
        print!("wb {wb} al {al} ");
        let n = 400usize;
        let program: Vec<_> = (0..n)
            .map(|i| {
                let mut instr = resource_test_instr(InstrSchedClass::DEFAULT);
                instr.pc = (i as u64) * u64::from(width);
                instr.width_bytes = width;
                instr
            })
            .collect();
        let cycles = issue_cycles(&model, &program);
        let last = *cycles.last().unwrap() as f64;
        println!(
            "width {width}: cyc/instr {:.4} ideal {:.4}",
            last / n as f64,
            f64::from(width) / 16.0
        );
    }
}

fn idiom_clone(instruction: &ScoreboardInstr) -> ScoreboardInstr {
    ScoreboardInstr {
        text: instruction.text.clone(),
        op_name: instruction.op_name.clone(),
        class: instruction.class,
        defs: instruction.defs.clone(),
        uses: instruction.uses.clone(),
        or_updates: instruction.or_updates.clone(),
        branch: None,
        pc: instruction.pc,
        width_bytes: instruction.width_bytes,
        mem: vec![],
        encoded_bytes: instruction.encoded_bytes.clone(),
        fusion_operands: instruction.fusion_operands.clone(),
        layout_known: instruction.layout_known,
        fusion_boundary: instruction.fusion_boundary,
    }
}

#[test]
fn nonmatching_fusion_rules_leave_existing_scheduler_unchanged() {
    let mut model = resource_test_model(&[ProcUnit {
        name: "P0",
        units: 1,
    }]);
    let class = InstrSchedClass {
        latency: 3,
        resources: &["P0"],
        ..InstrSchedClass::DEFAULT
    };
    let mut first = resource_test_instr(class);
    first.op_name = "producer".to_string();
    first.defs.push(("GPR".to_string(), 1));
    let mut second = resource_test_instr(class);
    second.op_name = "consumer".to_string();
    second.uses.push(("GPR".to_string(), 1));
    let program = [first, second];
    let config = TimingConfig {
        in_order: false,
        window: 2,
        mispredict_penalty: 0,
        unroll_stride: 0,
    };
    let mut plain_events = Recorder::default();
    let plain = run(
        &model,
        &program,
        4,
        &config,
        None,
        None,
        None,
        Some(&mut plain_events),
    );
    model.fusions = pair_rule("cmp", "jne");
    let mut ruled_events = Recorder::default();
    let ruled = run(
        &model,
        &program,
        4,
        &config,
        None,
        None,
        None,
        Some(&mut ruled_events),
    );
    assert_eq!(
        (ruled.cycles, ruled.instructions),
        (plain.cycles, plain.instructions)
    );
    assert_eq!(ruled_events, plain_events);
}

fn split_result_rule(
    rob_groups: &'static [FusionStageGroup],
    retire_groups: &'static [FusionStageGroup],
) -> &'static [FusionPattern] {
    const FIRST_OUTPUT: &[FusionOperandSelector] = &[FusionOperandSelector::AllOutputs(0)];
    const SECOND_OUTPUT: &[FusionOperandSelector] = &[FusionOperandSelector::AllOutputs(1)];
    const UOPS: &[FusionMicroOp] = &[
        FusionMicroOp {
            name: "early",
            routes: &[],
            inherit_routes: None,
            inputs: &[],
            outputs: FIRST_OUTPUT,
            depends_on: &[],
            read_cycle: 0,
            write_cycle: 1,
            memory: &[],
            control_steps: &[],
        },
        FusionMicroOp {
            name: "late",
            routes: &[],
            inherit_routes: None,
            inputs: &[],
            outputs: SECOND_OUTPUT,
            depends_on: &[],
            read_cycle: 0,
            write_cycle: 7,
            memory: &[],
            control_steps: &[],
        },
    ];
    let steps = Box::leak(Box::new([
        FusionStep {
            ops: &["early-def"],
        },
        FusionStep { ops: &["late-def"] },
    ]));
    Box::leak(Box::new([FusionPattern {
        name: "two-results",
        steps,
        guard: FusionExpr::True,
        schedule: FusionSchedule {
            decode_uops: 2,
            decoded_cache_uops: 2,
            decoder: None,
            decode_cycles: 1,
            rename_slots: 1,
            rob_slots: 2,
            retire_slots: 1,
            decode_groups: &[],
            rename_groups: &[],
            rob_groups,
            retire_groups,
            uops: UOPS,
        },
    }]))
}

#[test]
fn fused_outputs_wake_dependents_at_their_own_uop_times() {
    let mut model = resource_test_model(&[]);
    model.issue_width = 4;
    model.fusions = split_result_rule(&[], &[]);
    let mut first = dependency_test_instr(1, &[("GPR", 1)], &[], &[]);
    first.op_name = "early-def".to_string();
    let mut second = dependency_test_instr(1, &[("GPR", 2)], &[], &[]);
    second.op_name = "late-def".to_string();
    let early_reader = dependency_test_instr(1, &[], &[("GPR", 1)], &[]);
    let late_reader = dependency_test_instr(1, &[], &[("GPR", 2)], &[]);
    let issues = indexed_issue_cycles(&model, &[first, second, early_reader, late_reader]);
    assert_eq!(issues[0], 0);
    assert_eq!(issues[1], 0);
    assert_eq!(issues[2], 1);
    assert_eq!(issues[3], 7);
}

#[derive(Default)]
struct FusionCount(usize);
impl EventHandler for FusionCount {
    fn fused_group(
        &mut self,
        _first: usize,
        _members: usize,
        _name: &'static str,
        _decoded: u16,
        _execution: u16,
    ) {
        self.0 += 1;
    }
    fn render(&self) -> String {
        String::new()
    }
}

fn fusion_count(
    model: &MachineModel,
    program: &[ScoreboardInstr],
    iterations: usize,
    stride: u64,
) -> usize {
    let mut counter = FusionCount::default();
    run(
        model,
        program,
        iterations,
        &TimingConfig {
            in_order: false,
            window: 0,
            mispredict_penalty: 0,
            unroll_stride: stride,
        },
        None,
        None,
        None,
        Some(&mut counter),
    );
    counter.0
}

#[test]
fn unknown_guard_stops_ordered_fusion_selection() {
    use tir::backend::sched::{FusionOperandRef, FusionValue};
    let pair = pair_rule("cmp", "jne")[0];
    let missing = FusionValue::Operand(FusionOperandRef {
        step: 0,
        name: "missing",
    });
    let fallback = FusionPattern {
        name: "fallback",
        ..pair
    };
    let mut model = resource_test_model(&[]);
    let mut cmp = resource_test_instr(InstrSchedClass::DEFAULT);
    cmp.op_name = "cmp".to_string();
    let mut jne = resource_test_instr(InstrSchedClass::DEFAULT);
    jne.op_name = "jne".to_string();
    let overflow = FusionValue::Add(
        Box::leak(Box::new(FusionValue::Integer(i128::MAX))),
        Box::leak(Box::new(FusionValue::Integer(1))),
    );
    let divide_by_zero = FusionValue::Div(
        Box::leak(Box::new(FusionValue::Integer(1))),
        Box::leak(Box::new(FusionValue::Integer(0))),
    );
    for value in [missing, overflow, divide_by_zero] {
        let guarded = FusionPattern {
            name: "unknown",
            guard: FusionExpr::Eq(value, FusionValue::Integer(1)),
            ..pair
        };
        model.fusions = Box::leak(Box::new([guarded, fallback]));
        assert_eq!(fusion_count(&model, &[cmp.clone(), jne.clone()], 1, 0), 0);
    }
}

#[test]
fn layout_guard_checks_last_byte_and_each_repeated_instance() {
    let pair = pair_rule("cmp", "jne")[0];
    let mut model = resource_test_model(&[]);
    model.fusions = Box::leak(Box::new([FusionPattern {
        guard: FusionExpr::SameBlock {
            first: 0,
            second: 1,
            bytes: 4,
        },
        ..pair
    }]));
    let mut cmp = resource_test_instr(InstrSchedClass::DEFAULT);
    cmp.op_name = "cmp".to_string();
    cmp.layout_known = true;
    cmp.pc = 0;
    cmp.width_bytes = 2;
    let mut jne = resource_test_instr(InstrSchedClass::DEFAULT);
    jne.op_name = "jne".to_string();
    jne.layout_known = true;
    jne.pc = 2;
    jne.width_bytes = 1;
    assert_eq!(fusion_count(&model, &[cmp.clone(), jne.clone()], 2, 3), 1);
    cmp.pc = 2;
    jne.pc = 4;
    assert_eq!(fusion_count(&model, &[cmp, jne], 1, 0), 0);
}

#[test]
fn fusion_rename_cost_larger_than_width_spans_cycles() {
    let pair = pair_rule("cmp", "jne")[0];
    let schedule = FusionSchedule {
        rename_slots: 3,
        ..pair.schedule
    };
    let mut model = resource_test_model(&[]);
    model.issue_width = 1;
    model.fusions = Box::leak(Box::new([FusionPattern { schedule, ..pair }]));
    let mut cmp = resource_test_instr(InstrSchedClass::DEFAULT);
    cmp.op_name = "cmp".to_string();
    let mut jne = resource_test_instr(InstrSchedClass::DEFAULT);
    jne.op_name = "jne".to_string();
    let mut events = Recorder::default();
    let result = run(
        &model,
        &[cmp, jne],
        1,
        &TimingConfig {
            in_order: false,
            window: 0,
            mispredict_penalty: 0,
            unroll_stride: 0,
        },
        None,
        None,
        None,
        Some(&mut events),
    );
    assert_eq!(result.instructions, 2);
    assert_eq!(
        events
            .0
            .iter()
            .filter(|(event, _, _)| *event == 'D')
            .map(|(_, cycle, _)| *cycle)
            .collect::<Vec<_>>(),
        vec![2, 2]
    );
}

#[test]
fn ordinary_members_retire_together_after_a_fused_pair_is_selected() {
    let mut model = resource_test_model(&[]);
    model.issue_width = 2;
    let mut program = vec![dependency_test_instr(10, &[], &[], &[])];
    program.extend((0..3).map(|_| resource_test_instr(InstrSchedClass::DEFAULT)));
    let mut cmp = resource_test_instr(InstrSchedClass::DEFAULT);
    cmp.op_name = "cmp".to_string();
    let mut jne = resource_test_instr(InstrSchedClass::DEFAULT);
    jne.op_name = "jne".to_string();
    program.extend([cmp, jne]);
    let config = TimingConfig {
        in_order: false,
        window: 0,
        mispredict_penalty: 0,
        unroll_stride: 0,
    };
    let mut plain_events = Recorder::default();
    run(
        &model,
        &program,
        1,
        &config,
        None,
        None,
        None,
        Some(&mut plain_events),
    );
    model.fusions = pair_rule("cmp", "jne");
    let mut fused_events = Recorder::default();
    run(
        &model,
        &program,
        1,
        &config,
        None,
        None,
        None,
        Some(&mut fused_events),
    );
    let ordinary_retire_cycles = |events: &Recorder| {
        events
            .0
            .iter()
            .filter_map(|(kind, cycle, index)| (*kind == 'R' && *index < 4).then_some(*cycle))
            .collect::<Vec<_>>()
    };
    assert_eq!(ordinary_retire_cycles(&plain_events), vec![10; 4]);
    assert_eq!(ordinary_retire_cycles(&fused_events), vec![10; 4]);
}

#[test]
fn fused_retire_cost_above_width_spans_cycles() {
    let pair = pair_rule("cmp", "jne")[0];
    let mut model = resource_test_model(&[]);
    model.issue_width = 1;
    model.fusions = Box::leak(Box::new([FusionPattern {
        schedule: FusionSchedule {
            retire_slots: 3,
            ..pair.schedule
        },
        ..pair
    }]));
    let mut cmp = resource_test_instr(InstrSchedClass::DEFAULT);
    cmp.op_name = "cmp".to_string();
    let mut jne = resource_test_instr(InstrSchedClass::DEFAULT);
    jne.op_name = "jne".to_string();
    let mut events = Recorder::default();
    run(
        &model,
        &[cmp, jne],
        1,
        &TimingConfig {
            in_order: false,
            window: 0,
            mispredict_penalty: 0,
            unroll_stride: 0,
        },
        None,
        None,
        None,
        Some(&mut events),
    );
    let issue = events.0.iter().find(|(kind, _, _)| *kind == 'I').unwrap().1;
    let retire = events.0.iter().find(|(kind, _, _)| *kind == 'R').unwrap().1;
    assert_eq!(retire, issue + 3);
}

#[test]
fn split_retire_and_rob_groups_release_capacity_after_early_result() {
    const GROUPS: &[FusionStageGroup] = &[
        FusionStageGroup {
            steps: &[0],
            slots: 1,
        },
        FusionStageGroup {
            steps: &[1],
            slots: 1,
        },
    ];
    let mut model = resource_test_model(&[]);
    model.issue_width = 2;
    let mut first = dependency_test_instr(1, &[("GPR", 1)], &[], &[]);
    first.op_name = "early-def".to_string();
    let mut second = dependency_test_instr(1, &[("GPR", 2)], &[], &[]);
    second.op_name = "late-def".to_string();
    let tail = resource_test_instr(InstrSchedClass::DEFAULT);
    let program = [first, second, tail];
    let config = TimingConfig {
        in_order: false,
        window: 2,
        mispredict_penalty: 0,
        unroll_stride: 0,
    };
    model.fusions = split_result_rule(&[], &[]);
    let mut grouped = Recorder::default();
    run(
        &model,
        &program,
        1,
        &config,
        None,
        None,
        None,
        Some(&mut grouped),
    );
    model.fusions = split_result_rule(GROUPS, GROUPS);
    let mut split = Recorder::default();
    run(
        &model,
        &program,
        1,
        &config,
        None,
        None,
        None,
        Some(&mut split),
    );
    let dispatch_of_tail = |events: &Recorder| {
        events
            .0
            .iter()
            .find_map(|(kind, cycle, index)| (*kind == 'D' && *index == 2).then_some(*cycle))
            .unwrap()
    };
    assert_eq!(dispatch_of_tail(&grouped), 7);
    assert_eq!(dispatch_of_tail(&split), 1);
}

fn whole_sequence_rule(names: &'static [&'static str]) -> &'static [FusionPattern] {
    let steps: &'static [FusionStep] = Box::leak(
        names
            .iter()
            .map(|name| FusionStep {
                ops: Box::leak(Box::new([*name])),
            })
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    );
    let inputs: &'static [FusionOperandSelector] = Box::leak(
        (0..names.len())
            .map(FusionOperandSelector::AllInputs)
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    );
    let outputs: &'static [FusionOperandSelector] = Box::leak(
        (0..names.len())
            .map(FusionOperandSelector::AllOutputs)
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    );
    let memory: &'static [FusionMemoryRef] = Box::leak(
        (0..names.len())
            .map(|step| FusionMemoryRef { step, index: None })
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    );
    let controls: &'static [usize] =
        Box::leak((0..names.len()).collect::<Vec<_>>().into_boxed_slice());
    let uops = Box::leak(Box::new([FusionMicroOp {
        name: "whole",
        routes: &[],
        inherit_routes: Some(names.len() - 1),
        inputs,
        outputs,
        depends_on: &[],
        read_cycle: 0,
        write_cycle: 1,
        memory,
        control_steps: controls,
    }]));
    Box::leak(Box::new([FusionPattern {
        name: "whole",
        steps,
        guard: FusionExpr::True,
        schedule: FusionSchedule {
            decode_uops: 1,
            decoded_cache_uops: 1,
            decoder: None,
            decode_cycles: 1,
            rename_slots: 1,
            rob_slots: 1,
            retire_slots: 1,
            decode_groups: &[],
            rename_groups: &[],
            rob_groups: &[],
            retire_groups: &[],
            uops,
        },
    }]))
}

#[test]
fn singleton_and_three_step_patterns_preserve_architectural_events() {
    let mut model = resource_test_model(&[]);
    let make = |name: &str| {
        let mut slot = resource_test_instr(InstrSchedClass::DEFAULT);
        slot.op_name = name.to_string();
        slot
    };
    model.fusions = whole_sequence_rule(&["single"]);
    let one = [make("single")];
    assert_eq!(fusion_count(&model, &one, 2, 0), 2);
    let mut events = Recorder::default();
    let result = run(
        &model,
        &one,
        2,
        &TimingConfig {
            in_order: false,
            window: 0,
            mispredict_penalty: 0,
            unroll_stride: 0,
        },
        None,
        None,
        None,
        Some(&mut events),
    );
    assert_eq!(result.instructions, 2);
    assert_eq!(
        events.0.iter().filter(|(kind, _, _)| *kind == 'R').count(),
        2
    );

    model.fusions = whole_sequence_rule(&["a", "b", "c"]);
    let three = [make("a"), make("b"), make("c")];
    assert_eq!(fusion_count(&model, &three, 1, 0), 1);
    let mut boundary = three.clone();
    boundary[1].fusion_boundary = true;
    assert_eq!(fusion_count(&model, &boundary, 1, 0), 0);
}

#[test]
fn split_rename_groups_let_first_member_issue_before_second() {
    const RENAME: &[FusionStageGroup] = &[
        FusionStageGroup {
            steps: &[0],
            slots: 1,
        },
        FusionStageGroup {
            steps: &[1],
            slots: 1,
        },
    ];
    let mut model = resource_test_model(&[]);
    model.issue_width = 1;
    let pair = split_result_rule(&[], &[])[0];
    model.fusions = Box::leak(Box::new([FusionPattern {
        schedule: FusionSchedule {
            rename_groups: RENAME,
            ..pair.schedule
        },
        ..pair
    }]));
    let mut first = dependency_test_instr(1, &[("GPR", 1)], &[], &[]);
    first.op_name = "early-def".to_string();
    let mut second = dependency_test_instr(1, &[("GPR", 2)], &[], &[]);
    second.op_name = "late-def".to_string();
    let mut events = Recorder::default();
    run(
        &model,
        &[first, second],
        1,
        &TimingConfig {
            in_order: false,
            window: 0,
            mispredict_penalty: 0,
            unroll_stride: 0,
        },
        None,
        None,
        None,
        Some(&mut events),
    );
    let event_cycles = |event| {
        events
            .0
            .iter()
            .filter_map(|(kind, cycle, _)| (*kind == event).then_some(*cycle))
            .collect::<Vec<_>>()
    };
    assert_eq!(event_cycles('D'), vec![0, 1]);
    assert_eq!(event_cycles('I'), vec![0, 1]);
}

#[test]
fn fused_indexed_load_delays_only_its_result() {
    const INPUTS: &[FusionOperandSelector] = &[];
    const FIRST_OUTPUT: &[FusionOperandSelector] = &[FusionOperandSelector::AllOutputs(0)];
    const SECOND_OUTPUT: &[FusionOperandSelector] = &[FusionOperandSelector::AllOutputs(1)];
    const UOPS: &[FusionMicroOp] = &[
        FusionMicroOp {
            name: "load",
            routes: &[],
            inherit_routes: None,
            inputs: INPUTS,
            outputs: FIRST_OUTPUT,
            depends_on: &[],
            read_cycle: 2,
            write_cycle: 2,
            memory: &[FusionMemoryRef {
                step: 0,
                index: Some(0),
            }],
            control_steps: &[],
        },
        FusionMicroOp {
            name: "alu",
            routes: &[],
            inherit_routes: None,
            inputs: INPUTS,
            outputs: SECOND_OUTPUT,
            depends_on: &[],
            read_cycle: 0,
            write_cycle: 1,
            memory: &[],
            control_steps: &[],
        },
    ];
    let pair = split_result_rule(&[], &[])[0];
    let mut model = resource_test_model(&[]);
    model.issue_width = 4;
    model.fusions = Box::leak(Box::new([FusionPattern {
        schedule: FusionSchedule {
            uops: UOPS,
            ..pair.schedule
        },
        ..pair
    }]));
    let mut first = dependency_test_instr(1, &[("GPR", 1)], &[], &[]);
    first.op_name = "early-def".to_string();
    first.mem.push(MemAccess {
        addr: 0x1000,
        size: 4,
        is_write: false,
        kind: MemAccessKind::Data,
    });
    let mut second = dependency_test_instr(1, &[("GPR", 2)], &[], &[]);
    second.op_name = "late-def".to_string();
    let load_reader = dependency_test_instr(1, &[], &[("GPR", 1)], &[]);
    let alu_reader = dependency_test_instr(1, &[], &[("GPR", 2)], &[]);
    let cache = CacheParams {
        size: 64,
        ways: 1,
        line: 64,
        latency: 2,
        banks: 1,
        mshrs: 1,
    };
    let mut memory = MemorySystem::new(MemParams {
        l1i: cache,
        l1d: cache,
        l2: None,
        l3: None,
        dram_latency: 12,
        dram_streams: 1,
    });
    let mut events = Recorder::default();
    let result = run(
        &model,
        &[first, second, load_reader, alu_reader],
        1,
        &TimingConfig {
            in_order: false,
            window: 0,
            mispredict_penalty: 0,
            unroll_stride: 0,
        },
        None,
        None,
        Some(&mut memory),
        Some(&mut events),
    );
    let mut issues = vec![0; 4];
    for (kind, cycle, index) in &events.0 {
        if *kind == 'I' {
            issues[*index as usize] = *cycle;
        }
    }
    assert_eq!(result.instructions, 4);
    assert_eq!(memory.stats().l1d.accesses, 1);
    assert_eq!(
        issues[3],
        issues[0] + 1,
        "the independent ALU output should wake at one cycle"
    );
    assert!(
        issues[2] >= issues[0] + 14,
        "the load read two cycles after issue must wait for the miss: {issues:?}"
    );
}

#[test]
fn rename_bandwidth_is_charged_when_pressure_releases() {
    let pair = split_result_rule(&[], &[])[0];
    let mut model = resource_test_model(&[]);
    model.issue_width = 1;
    model.fusions = Box::leak(Box::new([FusionPattern {
        schedule: FusionSchedule {
            rename_slots: 2,
            ..pair.schedule
        },
        ..pair
    }]));
    let mut incumbent = dependency_test_instr(7, &[("GPR", 0)], &[], &[]);
    incumbent.op_name = "incumbent".to_string();
    let mut first = dependency_test_instr(1, &[("GPR", 1)], &[], &[]);
    first.op_name = "early-def".to_string();
    let mut second = dependency_test_instr(1, &[("GPR", 2)], &[], &[]);
    second.op_name = "late-def".to_string();
    let tail = resource_test_instr(InstrSchedClass::DEFAULT);
    let pressure = Prf {
        class_to_file: [("GPR".to_string(), "GPR".to_string())]
            .into_iter()
            .collect(),
        capacity: [("GPR".to_string(), 2)].into_iter().collect(),
    };
    let mut events = Recorder::default();
    run(
        &model,
        &[incumbent, first, second, tail],
        1,
        &TimingConfig {
            in_order: false,
            window: 4,
            mispredict_penalty: 0,
            unroll_stride: 0,
        },
        None,
        Some(&pressure),
        None,
        Some(&mut events),
    );
    let mut dispatch = vec![0; 4];
    for (kind, cycle, index) in &events.0 {
        if *kind == 'D' {
            dispatch[*index as usize] = *cycle;
        }
    }
    assert_eq!(dispatch, vec![0, 8, 8, 9]);
}

#[test]
fn register_overlap_guards_use_bit_ranges_and_unknown_stops_priority() {
    use tir::backend::sched::{FusionOperandRef, FusionValue};
    let pair = pair_rule("a", "b")[0];
    let left = FusionValue::Operand(FusionOperandRef { step: 0, name: "r" });
    let right = FusionValue::Operand(FusionOperandRef { step: 1, name: "r" });
    let mut model = resource_test_model(&[]);
    model.fusions = Box::leak(Box::new([
        FusionPattern {
            guard: FusionExpr::OverlapRegister(left, right),
            ..pair
        },
        FusionPattern {
            name: "fallback",
            ..pair
        },
    ]));
    let fact = |index, bit_range| FusionOperandFact {
        name: "r".to_string(),
        width_bits: Some(32),
        value: FusionOperandValue::Register {
            file: "GPR".to_string(),
            index,
            bit_range,
        },
    };
    let mut a = resource_test_instr(InstrSchedClass::DEFAULT);
    a.op_name = "a".to_string();
    a.fusion_operands.push(fact(0, Some((0, 64))));
    let mut b = resource_test_instr(InstrSchedClass::DEFAULT);
    b.op_name = "b".to_string();
    b.fusion_operands.push(fact(1, Some((32, 64))));
    assert_eq!(fusion_count(&model, &[a.clone(), b.clone()], 1, 0), 1);
    b.fusion_operands[0] = fact(1, Some((64, 96)));
    // A false first guard permits the lower-priority rule.
    assert_eq!(fusion_count(&model, &[a.clone(), b.clone()], 1, 0), 1);
    b.fusion_operands[0] = fact(1, None);
    assert_eq!(fusion_count(&model, &[a, b], 1, 0), 0);
}

#[test]
fn fused_branch_keeps_original_prediction_and_handler_identity() {
    let mut model = resource_test_model(&[]);
    model.issue_width = 2;
    model.fusions = pair_rule("cmp", "jne");
    let mut cmp = resource_test_instr(InstrSchedClass::DEFAULT);
    cmp.op_name = "cmp".to_string();
    let mut jne = resource_test_instr(InstrSchedClass::DEFAULT);
    jne.op_name = "jne".to_string();
    jne.branch = Some(BranchOutcome {
        pc: 0x20,
        target: 0x80,
        taken: true,
    });
    let successor = resource_test_instr(InstrSchedClass::DEFAULT);
    let mut predictor = AlwaysNotTaken;
    let mut events = Recorder::default();
    let result = run(
        &model,
        &[cmp, jne, successor],
        1,
        &TimingConfig {
            in_order: false,
            window: 0,
            mispredict_penalty: 4,
            unroll_stride: 0,
        },
        Some(&mut predictor),
        None,
        None,
        Some(&mut events),
    );
    assert_eq!(result.instructions, 3);
    assert_eq!(result.mispredicts, 1);
    assert!(events
        .0
        .iter()
        .any(|(kind, _, index)| *kind == 'M' && *index == 1));
    assert_eq!(
        events.0.iter().filter(|(kind, _, _)| *kind == 'R').count(),
        3
    );
    let successor_dispatch = events
        .0
        .iter()
        .find_map(|(kind, cycle, index)| (*kind == 'D' && *index == 2).then_some(*cycle))
        .unwrap();
    assert!(successor_dispatch >= 5);
}

#[test]
fn fusion_falls_back_when_runtime_outputs_alias_across_uops() {
    let pair = split_result_rule(&[], &[])[0];
    let uops = Box::leak(Box::new([
        pair.schedule.uops[0],
        FusionMicroOp {
            outputs: &[
                FusionOperandSelector::AllOutputs(0),
                FusionOperandSelector::AllOutputs(1),
            ],
            ..pair.schedule.uops[1]
        },
    ]));
    let mut model = resource_test_model(&[]);
    model.fusions = Box::leak(Box::new([FusionPattern {
        schedule: FusionSchedule {
            uops,
            ..pair.schedule
        },
        ..pair
    }]));
    let mut first = dependency_test_instr(1, &[("GPR", 1)], &[], &[]);
    first.op_name = "early-def".to_string();
    let mut second = dependency_test_instr(1, &[("GPR", 2)], &[], &[]);
    second.op_name = "late-def".to_string();
    assert_eq!(fusion_count(&model, &[first, second], 1, 0), 0);
}
