//! PBQP register allocation: coloring, spilling, aliasing and precoloring.

use std::collections::{BTreeSet, HashMap};

use tir::backend::abi::{
    AbiInfo, ClassifierKind, Overflow, PassSeq, SaveStyle, StackLayout, ValueKind,
};
use tir::backend::liveness::{self, Liveness, PhysReg};
use tir::backend::regalloc::{
    allocate, AllocConfig, AllocResult, RegAllocError, RegClassId, RegClassInfo, RegisterInfo,
    RegisterView,
};
use tir::builtin::{ops, IntegerType};
use tir::BlockHandle;
use tir::{Context, ValueId};

use super::fixtures::{r, register_info};

fn test_abi(info: &RegisterInfo, register_indices: &[u16]) -> &'static AbiInfo {
    let caller_saved = Box::leak(
        info.classes
            .iter()
            .flat_map(|class| {
                let class = RegClassId::new(class);
                register_indices
                    .iter()
                    .copied()
                    .map(move |index| (class, index))
            })
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    );
    let int_regs = Box::leak(
        caller_saved
            .iter()
            .copied()
            .take(2)
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    );
    Box::leak(Box::new(AbiInfo {
        name: "test",
        stack: StackLayout {
            align: 8,
            slot_size: 8,
            red_zone: 0,
            grows_down: true,
            save_style: SaveStyle::FrameSlots,
        },
        sp: (caller_saved[0].0, 1000),
        ra: None,
        fp: None,
        indirect_result: None,
        argument_group_alignment: None,
        argument_group_policy: None,
        args: Box::leak(
            vec![PassSeq {
                kind: ValueKind::Int,
                regs: int_regs,
                overflow: Overflow::Stack,
            }]
            .into_boxed_slice(),
        ),
        rets: Box::leak(
            vec![PassSeq {
                kind: ValueKind::Int,
                regs: &int_regs[..1],
                overflow: Overflow::Stack,
            }]
            .into_boxed_slice(),
        ),
        callee_saved: &[],
        caller_saved,
        reserved: &[],
        classifier: ClassifierKind::Sysv,
    }))
}

/// The `RegClassId` for class `name` in `info`. Register-class tables are
/// statics, so ids over the same table share one pointer identity.
fn id_of(info: &RegisterInfo, name: &str) -> RegClassId {
    info.class(name).unwrap()
}

fn liveness_with(vregs: &[u32], edges: &[(u32, u32)]) -> Liveness {
    let mut lv = Liveness::default();
    for &v in vregs {
        lv.vregs.insert(v);
        lv.vreg_class.insert(v, r());
    }
    for &(a, b) in edges {
        lv.interference.insert((a.min(b), a.max(b)));
    }
    lv
}

fn assigned(result: AllocResult) -> HashMap<u32, PhysReg> {
    match result {
        AllocResult::Assigned(map) => map,
        other => panic!("expected an assignment, got {other:?}"),
    }
}

fn addi(context: &Context, block: &BlockHandle, a: ValueId, b: ValueId, ty: tir::TypeId) -> u32 {
    block
        .append_op(ops::addi(context, a, b, ty).build())
        .result()
        .number()
}

// A value defined in the entry block and read in a successor block must not
// share a register with a temporary defined in the entry block after it: the
// cross-block liveness edge forces distinct registers. Without real CFG
// successors the two look non-interfering and the allocator may coalesce them,
// clobbering the cross-block value.
#[test]
fn cross_block_liveness_forces_distinct_registers() {
    let context = Context::with_default_dialects();
    let ty = IntegerType::new(&context, 64);
    let a = context.create_value(ty, None);
    let a_id = a.id();
    let entry = context.create_block(vec![a]);
    let succ = context.create_block(vec![]);

    let v = addi(&context, &entry, a_id, a_id, ty);
    let w = ValueId::from_number(addi(&context, &entry, a_id, a_id, ty));
    addi(&context, &entry, w, w, ty); // `w` dies in the entry block
    addi(&context, &succ, ValueId::from_number(v), a_id, ty); // `v` read across the edge

    let blocks = [entry.id(), succ.id()];
    let liveness = liveness::analyze(&context, &blocks, |blk| {
        if blk == entry.id() {
            vec![succ.id()]
        } else {
            vec![]
        }
    });

    let info = register_info();
    let precolor = HashMap::new();
    let map = assigned(
        allocate(&AllocConfig {
            info: &info,
            abi: test_abi(&info, &[0, 1, 2]),
            liveness: &liveness,
            precolor: &precolor,
            spill_cost: &|_| 100,
        })
        .unwrap(),
    );
    assert_ne!(
        map[&v],
        map[&w.number()],
        "a value live across a block edge must not reuse a later entry-block register",
    );
}

#[test]
fn mutually_live_vregs_get_distinct_registers() {
    let info = register_info();
    let liveness = liveness_with(&[1, 2, 3], &[(1, 2), (1, 3), (2, 3)]);
    let precolor = HashMap::new();
    let result = allocate(&AllocConfig {
        info: &info,
        abi: test_abi(&info, &[0, 1, 2]),
        liveness: &liveness,
        precolor: &precolor,
        spill_cost: &|_| 100,
    })
    .unwrap();

    let map = assigned(result);
    let regs: BTreeSet<u16> = map.values().map(|(_, i)| *i).collect();
    assert_eq!(
        regs.len(),
        3,
        "all three vregs must occupy distinct registers"
    );
}

#[test]
fn over_subscribed_clique_forces_a_spill() {
    let info = register_info();
    // Four mutually-live vregs, only three registers: exactly one must spill.
    let liveness = liveness_with(
        &[1, 2, 3, 4],
        &[(1, 2), (1, 3), (1, 4), (2, 3), (2, 4), (3, 4)],
    );
    let precolor = HashMap::new();
    let result = allocate(&AllocConfig {
        info: &info,
        abi: test_abi(&info, &[0, 1, 2]),
        liveness: &liveness,
        precolor: &precolor,
        spill_cost: &|_| 100,
    })
    .unwrap();

    match result {
        AllocResult::Spill(spilled) => assert_eq!(spilled.len(), 1),
        other => panic!("expected a spill, got {other:?}"),
    }
}

#[test]
fn spill_picks_the_cheapest_vreg() {
    let info = register_info();
    let liveness = liveness_with(
        &[1, 2, 3, 4],
        &[(1, 2), (1, 3), (1, 4), (2, 3), (2, 4), (3, 4)],
    );
    let precolor = HashMap::new();
    // vreg 4 is far cheaper to spill than the rest.
    let result = allocate(&AllocConfig {
        info: &info,
        abi: test_abi(&info, &[0, 1, 2]),
        liveness: &liveness,
        precolor: &precolor,
        spill_cost: &|v| if v == 4 { 1 } else { 1000 },
    })
    .unwrap();

    assert_eq!(result, AllocResult::Spill(vec![4]));
}

#[test]
fn precoloring_pins_a_vreg_and_repels_interferers() {
    let info = register_info();
    let liveness = liveness_with(&[1, 2], &[(1, 2)]);
    let mut precolor = HashMap::new();
    precolor.insert(1u32, (r(), 0u16));
    let result = allocate(&AllocConfig {
        info: &info,
        abi: test_abi(&info, &[0, 1, 2]),
        liveness: &liveness,
        precolor: &precolor,
        spill_cost: &|_| 100,
    })
    .unwrap();

    let map = assigned(result);
    assert_eq!(map[&1], (r(), 0));
    assert_ne!(
        map[&2].1, 0,
        "an interfering vreg cannot reuse the pinned register"
    );
}

#[test]
fn clique_larger_than_register_file_spills_the_excess() {
    // A k-register file and an n-vreg clique must spill exactly n - k of them.
    let info = register_info(); // 3 allocatable registers below
    let vregs: Vec<u32> = (0..6).collect();
    let mut edges = Vec::new();
    for i in 0..vregs.len() {
        for j in (i + 1)..vregs.len() {
            edges.push((vregs[i], vregs[j]));
        }
    }
    let liveness = liveness_with(&vregs, &edges);
    let precolor = HashMap::new();
    let result = allocate(&AllocConfig {
        info: &info,
        abi: test_abi(&info, &[0, 1, 2]),
        liveness: &liveness,
        precolor: &precolor,
        spill_cost: &|_| 100,
    })
    .unwrap();
    match result {
        AllocResult::Spill(s) => assert_eq!(s.len(), 6 - 3),
        other => panic!("expected spilling, got {other:?}"),
    }
}

#[test]
fn forbidden_register_is_avoided() {
    let info = register_info();
    let mut liveness = liveness_with(&[1], &[]);
    liveness
        .forbidden
        .entry(1)
        .or_default()
        .extend([(r(), 0u16), (r(), 1u16)]);
    let precolor = HashMap::new();
    let result = allocate(&AllocConfig {
        info: &info,
        abi: test_abi(&info, &[0, 1, 2]),
        liveness: &liveness,
        precolor: &precolor,
        spill_cost: &|_| 100,
    })
    .unwrap();

    let map = assigned(result);
    assert_eq!(map[&1], (r(), 2), "only the unforbidden register remains");
}

// Two register classes (`GPR` and `GPRsp`) over one shared file with a single
// allocatable register, mirroring AArch64's slot-31 aliasing.
static ALIASING_CLASSES: &[RegClassInfo] = &[
    RegClassInfo {
        name: "GPR",
        file: "GPR",
        registers: &[0, 1, 2, 3],
        group_width: 1,
        view: RegisterView {
            bit_offset: 0,
            merge: false,
        },
    },
    RegClassInfo {
        name: "GPRsp",
        file: "GPR",
        registers: &[0, 1, 2, 3],
        group_width: 1,
        view: RegisterView {
            bit_offset: 0,
            merge: false,
        },
    },
];

fn two_class_liveness(class1: RegClassId, class2: RegClassId) -> Liveness {
    let mut lv = Liveness::default();
    lv.vregs.insert(1);
    lv.vreg_class.insert(1, class1);
    lv.vregs.insert(2);
    lv.vreg_class.insert(2, class2);
    lv.interference.insert((1, 2));
    lv
}

#[test]
fn aliasing_classes_share_physical_registers() {
    // The two interfering vregs live in different classes that share one file
    // with a single register, so they cannot both be colored: one must spill.
    // Without file-based aliasing, `("GPR", 0)` and `("GPRsp", 0)` would look
    // distinct and the allocator would wrongly color both.
    let info = RegisterInfo {
        classes: ALIASING_CLASSES,
    };
    let liveness = two_class_liveness(id_of(&info, "GPR"), id_of(&info, "GPRsp"));
    let precolor = HashMap::new();
    let result = allocate(&AllocConfig {
        info: &info,
        abi: test_abi(&info, &[0]),
        liveness: &liveness,
        precolor: &precolor,
        spill_cost: &|_| 100,
    })
    .unwrap();

    match result {
        AllocResult::Spill(spilled) => assert_eq!(spilled.len(), 1),
        other => panic!("expected a spill from the shared file, got {other:?}"),
    }
}

#[test]
fn distinct_files_do_not_alias() {
    // Same shape, but the classes belong to different files, so both vregs can
    // independently take index 0.
    static CLASSES: &[RegClassInfo] = &[
        RegClassInfo {
            name: "A",
            file: "A",
            registers: &[0, 1, 2, 3],
            group_width: 1,
            view: RegisterView {
                bit_offset: 0,
                merge: false,
            },
        },
        RegClassInfo {
            name: "B",
            file: "B",
            registers: &[0, 1, 2, 3],
            group_width: 1,
            view: RegisterView {
                bit_offset: 0,
                merge: false,
            },
        },
    ];
    let info = RegisterInfo { classes: CLASSES };
    let liveness = two_class_liveness(id_of(&info, "A"), id_of(&info, "B"));
    let precolor = HashMap::new();
    let result = allocate(&AllocConfig {
        info: &info,
        abi: test_abi(&info, &[0]),
        liveness: &liveness,
        precolor: &precolor,
        spill_cost: &|_| 100,
    })
    .unwrap();

    let map = assigned(result);
    assert_eq!(map[&1], (id_of(&info, "A"), 0));
    assert_eq!(map[&2], (id_of(&info, "B"), 0));
}

#[test]
fn group_registers_interfere_by_span() {
    // An RVV-style LMUL=2 group class: a `VRM2` register covers two `VR`
    // indices, so an interfering single-register vreg must land outside the
    // group's span. With only v0..v2 available, the group takes (VRM2, 0)
    // = v0..v1 and the scalar is pushed to v2 (not v1, which overlaps).
    static CLASSES: &[RegClassInfo] = &[
        RegClassInfo {
            name: "VR",
            file: "VR",
            registers: &[0, 1, 2, 3],
            group_width: 1,
            view: RegisterView {
                bit_offset: 0,
                merge: false,
            },
        },
        RegClassInfo {
            name: "VRM2",
            file: "VR",
            registers: &[0, 1, 2, 3],
            group_width: 2,
            view: RegisterView {
                bit_offset: 0,
                merge: false,
            },
        },
    ];
    let info = RegisterInfo { classes: CLASSES };
    let vrm2 = id_of(&info, "VRM2");
    let vr = id_of(&info, "VR");
    assert!(info.phys_overlap(&(vrm2, 0), &(vr, 1)));
    assert!(!info.phys_overlap(&(vrm2, 0), &(vr, 2)));
    // The overlap API is also exposed directly on the class handle.
    assert!(vrm2.overlaps(0, vr, 1));
    assert!(!vrm2.overlaps(0, vr, 2));

    let liveness = two_class_liveness(vrm2, vr);
    let precolor = HashMap::new();
    let result = allocate(&AllocConfig {
        info: &info,
        abi: test_abi(&info, &[0, 1, 2]),
        liveness: &liveness,
        precolor: &precolor,
        spill_cost: &|_| 100,
    })
    .unwrap();

    let map = assigned(result);
    assert_eq!(map[&1].1 % 2, 0);
    assert!(!info.phys_overlap(&map[&1], &map[&2]));
}

#[test]
fn forbidden_register_aliases_across_classes() {
    // A `GPRsp` vreg forbidding `("GPR", 0)` — a clobber expressed through the
    // aliasing base class — must avoid index 0 and take the other register.
    static CLASSES: &[RegClassInfo] = &[
        RegClassInfo {
            name: "GPR",
            file: "GPR",
            registers: &[0, 1, 2, 3],
            group_width: 1,
            view: RegisterView {
                bit_offset: 0,
                merge: false,
            },
        },
        RegClassInfo {
            name: "GPRsp",
            file: "GPR",
            registers: &[0, 1, 2, 3],
            group_width: 1,
            view: RegisterView {
                bit_offset: 0,
                merge: false,
            },
        },
    ];
    let info = RegisterInfo { classes: CLASSES };
    let mut liveness = Liveness::default();
    liveness.vregs.insert(1);
    liveness.vreg_class.insert(1, id_of(&info, "GPRsp"));
    liveness
        .forbidden
        .entry(1)
        .or_default()
        .insert((id_of(&info, "GPR"), 0u16));
    let precolor = HashMap::new();
    let result = allocate(&AllocConfig {
        info: &info,
        abi: test_abi(&info, &[0, 1]),
        liveness: &liveness,
        precolor: &precolor,
        spill_cost: &|_| 100,
    })
    .unwrap();

    let map = assigned(result);
    assert_eq!(
        map[&1],
        (id_of(&info, "GPRsp"), 1),
        "a forbidden index aliases across the shared file"
    );
}

// `GPR` (whole file) and its REX-free subclass `GPRlow` (indices 0..1).
static SUBCLASS_CLASSES: &[RegClassInfo] = &[
    RegClassInfo {
        name: "GPR",
        file: "GPR",
        registers: &[0, 1, 2, 3],
        group_width: 1,
        view: RegisterView {
            bit_offset: 0,
            merge: false,
        },
    },
    RegClassInfo {
        name: "GPRlow",
        file: "GPR",
        registers: &[0, 1],
        group_width: 1,
        view: RegisterView {
            bit_offset: 0,
            merge: false,
        },
    },
];

// A vreg pinned through the wide class but read by an operand of a narrow
// subclass must be colored from the narrow class: the pin says *which*
// register, the operand says which registers the instruction can encode, and
// both hold. Taking the pin's class instead would hand out an index the
// encoding drops, silently naming a different register.
#[test]
fn precolor_does_not_widen_a_narrow_operand_class() {
    let info = RegisterInfo {
        classes: SUBCLASS_CLASSES,
    };
    let low = id_of(&info, "GPRlow");
    let wide = id_of(&info, "GPR");
    let mut liveness = Liveness::default();
    liveness.vregs.insert(1);
    liveness.vreg_class.insert(1, low);
    let precolor = HashMap::from([(1u32, (wide, 1u16))]);

    let map = assigned(
        allocate(&AllocConfig {
            info: &info,
            abi: test_abi(&info, &[0, 1, 2, 3]),
            liveness: &liveness,
            precolor: &precolor,
            spill_cost: &|_| 100,
        })
        .unwrap(),
    );
    assert_eq!(map[&1], (low, 1));
}

// The pin names a register the narrow operand class cannot encode. There is no
// legal color, so the allocator must fail loudly (the caller breaks the
// constraint with a copy) rather than widen the class.
#[test]
fn precolor_outside_narrow_class_is_infeasible() {
    let info = RegisterInfo {
        classes: SUBCLASS_CLASSES,
    };
    let low = id_of(&info, "GPRlow");
    let wide = id_of(&info, "GPR");
    let mut liveness = Liveness::default();
    liveness.vregs.insert(1);
    liveness.vreg_class.insert(1, low);
    let precolor = HashMap::from([(1u32, (wide, 3u16))]);

    assert_eq!(
        allocate(&AllocConfig {
            info: &info,
            abi: test_abi(&info, &[0, 1, 2, 3]),
            liveness: &liveness,
            precolor: &precolor,
            spill_cost: &|_| 100,
        }),
        Err(RegAllocError::Infeasible(1)),
    );
}

// A vreg constrained by classes that overlap without containing each other
// (an x86 value read both by a REX-free operand and as a SIB index) may only
// be colored from the indices every class encodes.
#[test]
fn overlapping_class_constraints_restrict_the_allocation_order() {
    let info = RegisterInfo {
        classes: SUBCLASS_CLASSES,
    };
    let low = id_of(&info, "GPRlow");
    let mut liveness = Liveness::default();
    liveness.vregs.insert(1);
    liveness.vreg_class.insert(1, low);
    // `GPRlow` encodes {0, 1}; the other constraint drops index 0.
    liveness
        .allowed_indices
        .insert(1, BTreeSet::from([1, 2, 3]));
    let precolor = HashMap::new();

    let map = assigned(
        allocate(&AllocConfig {
            info: &info,
            abi: test_abi(&info, &[0, 1, 2, 3]),
            liveness: &liveness,
            precolor: &precolor,
            spill_cost: &|_| 100,
        })
        .unwrap(),
    );
    assert_eq!(map[&1], (low, 1));
}

// A vreg with contradictory class constraints is an allocation error, not a
// silent choice of one of them.
#[test]
fn conflicting_class_constraints_fail_allocation() {
    let info = RegisterInfo {
        classes: SUBCLASS_CLASSES,
    };
    let low = id_of(&info, "GPRlow");
    let wide = id_of(&info, "GPR");
    let mut liveness = Liveness::default();
    liveness.vregs.insert(1);
    liveness.vreg_class.insert(1, wide);
    liveness.class_conflicts.insert(1, (wide, low));
    let precolor = HashMap::new();

    assert_eq!(
        allocate(&AllocConfig {
            info: &info,
            abi: test_abi(&info, &[0, 1, 2, 3]),
            liveness: &liveness,
            precolor: &precolor,
            spill_cost: &|_| 100,
        }),
        Err(RegAllocError::ClassConflict(1)),
    );
}
