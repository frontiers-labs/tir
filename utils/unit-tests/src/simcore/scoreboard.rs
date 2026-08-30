use std::collections::{HashMap, VecDeque};

use tir::backend::sched::{
    DecodedCache, Decoder, Forward, Frontend, FrontendDecode, FrontendFetch, InstrSchedClass,
    MachineModel, MicroOp, ProcUnit, ResourceRoute, ResourceUse,
};
use tir_sim::predictor::{AlwaysNotTaken, BranchPredictor};
use tir_sim::scoreboard::*;

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
                operands_ready =
                    operands_ready.max(issue[j] + edge_latency(model, producer, &slot.class));
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
                branch,
                pc: 0,
                width_bytes: 1,
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
        branch: None,
        pc: 0,
        width_bytes: 1,
        mem: vec![],
    }
}

fn issue_cycles(model: &MachineModel, program: &[ScoreboardInstr]) -> Vec<u64> {
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
    events
        .0
        .iter()
        .filter_map(|(event, cycle, _)| (*event == 'I').then_some(*cycle))
        .collect()
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
    model.fusions = &[tir::backend::sched::FusionGroup {
        first: &["cmp"],
        second: &["jne"],
    }];

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
        branch: None,
        pc: instruction.pc,
        width_bytes: instruction.width_bytes,
        mem: vec![],
    }
}
