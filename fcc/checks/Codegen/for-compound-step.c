// RUN: fcc compile --stage ir -o - %s | filecheck %s

// A `+=` step counts like any other: the constant it adds becomes the loop's
// step, and a bound read from a parameter's slot is read once before the loop,
// off that slot's own chain — the counter's slot has a chain of its own, so
// nothing orders the two reads against each other.

int advance(int limit) {
    int value;
    for (value = 0; value < limit; value += 2) {
    }
    return value;
}

// CHECK: %[[ST:[0-9]+]] = constant {value = 2} : !i32
// CHECK: state(%[[E:[0-9]+]]) = state.entry_state
// CHECK-NEXT: state(%[[PCHAIN:[0-9]+]], %[[CCHAIN:[0-9]+]], %{{[0-9]+}}) = state.split state(%[[E]])
// CHECK-NEXT: state(%[[PARAM:[0-9]+]]) = ptr.store %{{[0-9]+}}, %[[PSLOT:[0-9]+]] state(%[[PCHAIN]])
// CHECK-NEXT: state(%[[INIT:[0-9]+]]) = ptr.store %{{[0-9]+}}, %[[CSLOT:[0-9]+]] state(%[[CCHAIN]])
// CHECK-NEXT: %[[LB:[0-9]+]], state(%{{[0-9]+}}) = ptr.load %[[CSLOT]] state(%[[INIT]]) : !i32
// CHECK-NEXT: %[[UB:[0-9]+]], state(%{{[0-9]+}}) = ptr.load %[[PSLOT]] state(%[[PARAM]]) : !i32
// CHECK: scf.for %{{[0-9]+}} = %[[LB]] to %[[UB]] step %[[ST]] (
