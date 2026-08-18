//! Backend liveness: class constraints, interference and fixpoint.

use std::collections::BTreeSet;

use tir::backend::liveness::analyze;
use tir::backend::regalloc::{RegClassId, RegClassInfo, RegisterView};
use tir::builtin::{ops, IntegerType};
use tir::{BlockHandle, Context, Operation, TypeId, ValueId};

use super::fixtures::r;

tir::helpers::operation! {
    PhysDefOp {
        name: "phys_def",
        dialect: "test",
        attributes: A {
            r: "Register",
        },
        interfaces: [tir::attributes::RegisterSemantics],
    }
}

impl tir::attributes::RegisterSemantics for PhysDefOp {
    fn attribute_roles(&self) -> &'static [(&'static str, tir::attributes::AttributeRole)] {
        &[("r", tir::attributes::AttributeRole::Def)]
    }
}

tir::helpers::operation! {
    PhysUseOp {
        name: "phys_use",
        dialect: "test",
        attributes: A {
            r: "Register",
        },
        interfaces: [tir::attributes::RegisterSemantics],
    }
}

impl tir::attributes::RegisterSemantics for PhysUseOp {
    fn attribute_roles(&self) -> &'static [(&'static str, tir::attributes::AttributeRole)] {
        &[("r", tir::attributes::AttributeRole::Use)]
    }
}

// A subclass of `R` over the same file and view: fewer encodable registers.
static R_LOW_CLASS: RegClassInfo = RegClassInfo {
    name: "Rlow",
    file: "R",
    registers: &[0, 1],
    group_width: 1,
    view: RegisterView {
        bit_offset: 0,
        merge: false,
    },
};

// Same file and index set as `Rlow`, but a different architectural view (an
// x86 high-byte class): no register satisfies both constraints.
static R_HIGH_CLASS: RegClassInfo = RegClassInfo {
    name: "Rhigh",
    file: "R",
    registers: &[0, 1],
    group_width: 1,
    view: RegisterView {
        bit_offset: 8,
        merge: true,
    },
};

fn r_low() -> RegClassId {
    RegClassId::new(&R_LOW_CLASS)
}

fn r_high() -> RegClassId {
    RegClassId::new(&R_HIGH_CLASS)
}

// Append an op reading virtual register `id` through class `class`.
fn vreg_use(context: &Context, block: &BlockHandle, id: u32, class: RegClassId) {
    use tir::attributes::{AttributeValue, RegisterAttr};

    PhysUseOp::register_interfaces(context);
    let register = AttributeValue::Register(RegisterAttr::Virtual {
        id,
        class: Some(class),
    });
    let op = PhysUseOpBuilder::new(context)
        .attr("r", register)
        .build()
        .id();
    block.append(op);
}

// A vreg constrained by two classes over one file and view must end up in the
// narrower one — the wider constraint is satisfied by every register of the
// narrower, but not the other way round. Order of appearance is irrelevant.
#[test]
fn narrower_class_constraint_wins() {
    for wide_first in [true, false] {
        let context = Context::with_default_dialects();
        let ty = IntegerType::new(&context, 64);
        let a = context.create_value(ty, None);
        let a_id = a.id().number();
        let block = context.create_block(vec![a]);

        if wide_first {
            vreg_use(&context, &block, a_id, r());
            vreg_use(&context, &block, a_id, r_low());
        } else {
            vreg_use(&context, &block, a_id, r_low());
            vreg_use(&context, &block, a_id, r());
        }

        let liveness = analyze(&context, &[block.id()], |_| Vec::new());
        assert_eq!(
            liveness.vreg_class.get(&a_id),
            Some(&r_low()),
            "the narrower operand class must survive (wide first: {wide_first})",
        );
        assert_eq!(
            liveness.allowed_indices.get(&a_id),
            Some(&BTreeSet::from([0, 1])),
        );
        assert!(liveness.class_conflicts.is_empty());
    }
}

// Two classes over one view where neither contains the other (x86 `GPR32low`,
// which includes esp, and `GPRaddrIndex`, which excludes rsp but reaches r8+):
// the vreg is allocatable from the indices both encode, and nothing else.
#[test]
fn overlapping_classes_intersect_their_indices() {
    static R_MID_CLASS: RegClassInfo = RegClassInfo {
        name: "Rmid",
        file: "R",
        registers: &[1, 2, 3],
        group_width: 1,
        view: RegisterView {
            bit_offset: 0,
            merge: false,
        },
    };
    let context = Context::with_default_dialects();
    let ty = IntegerType::new(&context, 64);
    let a = context.create_value(ty, None);
    let a_id = a.id().number();
    let block = context.create_block(vec![a]);

    vreg_use(&context, &block, a_id, r_low()); // {0, 1}
    vreg_use(&context, &block, a_id, RegClassId::new(&R_MID_CLASS)); // {1, 2, 3}

    let liveness = analyze(&context, &[block.id()], |_| Vec::new());
    assert!(liveness.class_conflicts.is_empty());
    assert_eq!(
        liveness.allowed_indices.get(&a_id),
        Some(&BTreeSet::from([1])),
    );
}

// Classes over one view with no register in common cannot both be honored.
#[test]
fn disjoint_classes_over_one_view_are_reported() {
    static R_OTHER_CLASS: RegClassInfo = RegClassInfo {
        name: "Rother",
        file: "R",
        registers: &[2, 3],
        group_width: 1,
        view: RegisterView {
            bit_offset: 0,
            merge: false,
        },
    };
    let context = Context::with_default_dialects();
    let ty = IntegerType::new(&context, 64);
    let a = context.create_value(ty, None);
    let a_id = a.id().number();
    let block = context.create_block(vec![a]);

    vreg_use(&context, &block, a_id, r_low()); // {0, 1}
    vreg_use(&context, &block, a_id, RegClassId::new(&R_OTHER_CLASS)); // {2, 3}

    let liveness = analyze(&context, &[block.id()], |_| Vec::new());
    assert!(liveness.class_conflicts.contains_key(&a_id));
}

// Two classes where neither is a subclass of the other (here: different views
// of the same file) cannot both be honored; the allocator must be told rather
// than silently keeping one.
#[test]
fn incompatible_class_constraints_are_reported() {
    let context = Context::with_default_dialects();
    let ty = IntegerType::new(&context, 64);
    let a = context.create_value(ty, None);
    let a_id = a.id().number();
    let block = context.create_block(vec![a]);

    vreg_use(&context, &block, a_id, r_low());
    vreg_use(&context, &block, a_id, r_high());

    let liveness = analyze(&context, &[block.id()], |_| Vec::new());
    assert_eq!(
        liveness.class_conflicts.get(&a_id),
        Some(&(r_low(), r_high())),
    );
}

// `addi %a, %b` whose fresh result names a new virtual register (a def), with
// its two operands read as uses — enough for liveness, which resolves builtin
// SSA ops positionally.
fn addi(context: &Context, block: &BlockHandle, a: ValueId, b: ValueId, ty: TypeId) -> ValueId {
    block
        .append_op(ops::addi(context, a, b, ty).build())
        .result()
}

// Two defs in the entry block where the first is used only in a successor
// block: the two entry defs interfere iff the successor edge is wired, because
// that is what keeps the first value live across the second's def. With the
// edge dropped (the old `|_| Vec::new()`), the first value looks dead at its
// def and the allocator is free to reuse its register — the miscompile.
#[test]
fn cross_block_def_interferes_only_with_wired_successors() {
    let context = Context::with_default_dialects();
    let ty = IntegerType::new(&context, 64);
    let a = context.create_value(ty, None);
    let a_id = a.id();
    let entry = context.create_block(vec![a]);
    let succ = context.create_block(vec![]);

    // `v` is used only in the successor (so it is live across the edge); `w`
    // is defined after `v` and dies inside the entry block (consumed by `u`).
    // Their interference therefore hinges entirely on `v` being live-out.
    let v = addi(&context, &entry, a_id, a_id, ty);
    let w = addi(&context, &entry, a_id, a_id, ty);
    addi(&context, &entry, w, w, ty);
    addi(&context, &succ, v, a_id, ty);

    let blocks = [entry.id(), succ.id()];
    let with_edge = analyze(&context, &blocks, |blk| {
        if blk == entry.id() {
            vec![succ.id()]
        } else {
            vec![]
        }
    });
    assert!(
        with_edge.interferes(v.number(), w.number()),
        "a value live across a later def must interfere with it",
    );
    assert!(
        with_edge.live_in[&succ.id()].contains(&v.number()),
        "the cross-block value is live into its using block",
    );

    let no_edge = analyze(&context, &blocks, |_| Vec::new());
    assert!(
        !no_edge.interferes(v.number(), w.number()),
        "without the CFG edge the bug hides the interference (regression guard)",
    );
}

// Diamond: entry defines a value used only at the merge, so it is live-through
// both arms and must interfere with every def on either arm.
#[test]
fn diamond_live_through_interferes_on_both_arms() {
    let context = Context::with_default_dialects();
    let ty = IntegerType::new(&context, 64);
    let a = context.create_value(ty, None);
    let a_id = a.id();
    let entry = context.create_block(vec![a]);
    let left = context.create_block(vec![]);
    let right = context.create_block(vec![]);
    let merge = context.create_block(vec![]);

    let v = addi(&context, &entry, a_id, a_id, ty);
    let la = addi(&context, &left, a_id, a_id, ty);
    let ra = addi(&context, &right, a_id, a_id, ty);
    addi(&context, &merge, v, a_id, ty);

    let blocks = [entry.id(), left.id(), right.id(), merge.id()];
    let liveness = analyze(&context, &blocks, |blk| {
        if blk == entry.id() {
            vec![left.id(), right.id()]
        } else if blk == left.id() || blk == right.id() {
            vec![merge.id()]
        } else {
            vec![]
        }
    });

    assert!(liveness.live_in[&left.id()].contains(&v.number()));
    assert!(liveness.live_in[&right.id()].contains(&v.number()));
    assert!(
        liveness.interferes(v.number(), la.number()),
        "live-through value must interfere with the left arm's def",
    );
    assert!(
        liveness.interferes(v.number(), ra.number()),
        "live-through value must interfere with the right arm's def",
    );
}

// Append an op that reads (`is_def == false`) or writes (`is_def == true`) the
// physical register `class[index]` via a role-tagged register attribute.
fn phys_op(context: &Context, block: &BlockHandle, class: RegClassId, index: u16, is_def: bool) {
    use tir::attributes::{AttributeValue, RegisterAttr};

    // The test dialect is never registered, so hook the role interfaces in
    // directly.
    PhysDefOp::register_interfaces(context);
    PhysUseOp::register_interfaces(context);
    let register = AttributeValue::Register(RegisterAttr::Physical { class, index });
    let id = if is_def {
        PhysDefOpBuilder::new(context)
            .attr("r", register)
            .build()
            .id()
    } else {
        PhysUseOpBuilder::new(context)
            .attr("r", register)
            .build()
            .id()
    };
    block.append(id);
}

// A fixed-register read protocol: `def P; def v1; use P; use v1`. `v1` is live
// across the read of the physical register `P`, so it must not be colored `P` —
// otherwise the allocator could park it in `P` between `P`'s def and this read.
#[test]
fn physical_read_forbids_live_vreg() {
    let context = Context::with_default_dialects();
    let ty = IntegerType::new(&context, 64);
    let a = context.create_value(ty, None);
    let a_id = a.id();
    let block = context.create_block(vec![a]);

    phys_op(&context, &block, r(), 0, true); // def P
    let v1 = addi(&context, &block, a_id, a_id, ty); // def v1 (live across the read)
    phys_op(&context, &block, r(), 0, false); // use P
    addi(&context, &block, v1, a_id, ty); // use v1

    let liveness = analyze(&context, &[block.id()], |_| Vec::new());

    assert!(
        liveness.forbidden[&v1.number()].contains(&(r(), 0)),
        "a vreg live across a physical-register read must be forbidden from it",
    );
}

// A back edge (a loop): the fixpoint must converge, and a value defined in the
// header and read inside the body stays live around the edge.
#[test]
fn loop_back_edge_converges() {
    let context = Context::with_default_dialects();
    let ty = IntegerType::new(&context, 64);
    let a = context.create_value(ty, None);
    let a_id = a.id();
    let header = context.create_block(vec![a]);
    let body = context.create_block(vec![]);

    let carried = addi(&context, &header, a_id, a_id, ty);
    addi(&context, &body, carried, a_id, ty);

    // header -> body -> header (back edge).
    let blocks = [header.id(), body.id()];
    let liveness = analyze(&context, &blocks, |blk| {
        if blk == header.id() {
            vec![body.id()]
        } else {
            vec![header.id()]
        }
    });

    assert!(
        liveness.live_in[&body.id()].contains(&carried.number()),
        "the header-defined value is live into the loop body",
    );
}
