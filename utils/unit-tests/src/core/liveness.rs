//! The register classes a machine function constrains its values to.

use std::collections::BTreeSet;

use tir::backend::liveness::Classes;
use tir::backend::regalloc::{RegClassId, RegClassInfo};
use tir::backend::{RegClassType, RegPort};
use tir::{BlockHandle, Context, Operation, ValueId};

use super::fixtures::{machine_op, r, r_high, reg_class};

// The test ops: each names one register slot. A slot's class is a per-opcode
// fact, so there is one op per class the tests constrain a value through; the
// value's own class is its type.
macro_rules! slot_op {
    ($op:ident, $ports:ident, $name:tt, $class:expr, $def:literal) => {
        static $ports: [RegPort; 1] = [RegPort {
            name: "r",
            class: $class,
            def: $def,
            tied_to: None,
        }];

        machine_op!($op, "test", $name, r, &$ports, &[]);
    };
}

slot_op!(UseROp, USE_R_PORTS, "use_r", Some(r()), false);
slot_op!(UseRlowOp, USE_RLOW_PORTS, "use_rlow", Some(r_low()), false);
slot_op!(
    UseRhighOp,
    USE_RHIGH_PORTS,
    "use_rhigh",
    Some(r_high()),
    false
);
slot_op!(UseRmidOp, USE_RMID_PORTS, "use_rmid", Some(r_mid()), false);
slot_op!(
    UseROtherOp,
    USE_ROTHER_PORTS,
    "use_rother",
    Some(r_other()),
    false
);

// A subclass of `R` over the same file and view: fewer encodable registers.
static R_LOW_CLASS: RegClassInfo = reg_class("Rlow", "R", &[0, 1], 1, 0, false);

const fn r_low() -> RegClassId {
    RegClassId::new(&R_LOW_CLASS)
}

// Two classes over one view where neither contains the other (x86 `GPR32low`,
// which includes esp, and `GPRaddrIndex`, which excludes rsp but reaches r8+).
static R_MID_CLASS: RegClassInfo = reg_class("Rmid", "R", &[1, 2, 3], 1, 0, false);

const fn r_mid() -> RegClassId {
    RegClassId::new(&R_MID_CLASS)
}

// Over one view with `Rlow`, but sharing no register with it.
static R_OTHER_CLASS: RegClassInfo = reg_class("Rother", "R", &[2, 3], 1, 0, false);

const fn r_other() -> RegClassId {
    RegClassId::new(&R_OTHER_CLASS)
}

// A value of class `class`, the only thing its type says about it.
fn reg_value(context: &Context, class: RegClassId) -> ValueId {
    context
        .create_value(RegClassType::new(context, class), None)
        .id()
}

// Append an op reading `value` through the register slot of class `class`.
fn vreg_use(context: &Context, block: &BlockHandle, value: ValueId, class: RegClassId) {
    macro_rules! read {
        ($op:ident, $builder:ident) => {{
            $op::register_interfaces(context);
            $builder::new(context).r(value).build().id()
        }};
    }
    let op = match class.name() {
        "R" => read!(UseROp, UseROpBuilder),
        "Rlow" => read!(UseRlowOp, UseRlowOpBuilder),
        "Rhigh" => read!(UseRhighOp, UseRhighOpBuilder),
        "Rmid" => read!(UseRmidOp, UseRmidOpBuilder),
        "Rother" => read!(UseROtherOp, UseROtherOpBuilder),
        other => unreachable!("no test op reads through {other}"),
    };
    block.append(op);
}

// A value constrained by two classes over one file and view must end up in the
// narrower one — the wider constraint is satisfied by every register of the
// narrower, but not the other way round. Order of appearance is irrelevant.
#[test]
fn narrower_class_constraint_wins() {
    for wide_first in [true, false] {
        let context = Context::with_default_dialects();
        let a = reg_value(&context, r());
        let block = context.create_block(vec![context.get_value(a)]);

        if wide_first {
            vreg_use(&context, &block, a, r());
            vreg_use(&context, &block, a, r_low());
        } else {
            vreg_use(&context, &block, a, r_low());
            vreg_use(&context, &block, a, r());
        }

        let classes = Classes::of(&context, &[block.id()]);
        assert_eq!(
            classes.vreg_class.get(&a.number()),
            Some(&r_low()),
            "the narrower operand class must survive (wide first: {wide_first})",
        );
        assert_eq!(
            classes.allowed_indices.get(&a.number()),
            Some(&BTreeSet::from([0, 1])),
        );
        assert!(classes.class_conflicts.is_empty());
    }
}

// Two classes over one view where neither contains the other: the value is
// allocatable from the indices both encode, and nothing else.
#[test]
fn overlapping_classes_intersect_their_indices() {
    let context = Context::with_default_dialects();
    let a = reg_value(&context, r());
    let block = context.create_block(vec![context.get_value(a)]);

    vreg_use(&context, &block, a, r_low()); // {0, 1}
    vreg_use(&context, &block, a, r_mid()); // {1, 2, 3}

    let classes = Classes::of(&context, &[block.id()]);
    assert!(classes.class_conflicts.is_empty());
    assert_eq!(
        classes.allowed_indices.get(&a.number()),
        Some(&BTreeSet::from([1])),
    );
}

// Classes over one view with no register in common cannot both be honored.
#[test]
fn disjoint_classes_over_one_view_are_reported() {
    let context = Context::with_default_dialects();
    let a = reg_value(&context, r());
    let block = context.create_block(vec![context.get_value(a)]);

    vreg_use(&context, &block, a, r_low()); // {0, 1}
    vreg_use(&context, &block, a, r_other()); // {2, 3}

    let classes = Classes::of(&context, &[block.id()]);
    assert!(classes.class_conflicts.contains_key(&a.number()));
}

// A value's own class and the class of a slot reading it must share a view;
// here they do not (an x86 high-byte slot reading an offset-0 value), and the
// allocator must be told rather than silently keeping one.
#[test]
fn incompatible_class_constraints_are_reported() {
    let context = Context::with_default_dialects();
    let a = reg_value(&context, r_low());
    let block = context.create_block(vec![context.get_value(a)]);

    vreg_use(&context, &block, a, r_high());

    let classes = Classes::of(&context, &[block.id()]);
    assert_eq!(
        classes.class_conflicts.get(&a.number()),
        Some(&(r_low(), r_high())),
    );
}
