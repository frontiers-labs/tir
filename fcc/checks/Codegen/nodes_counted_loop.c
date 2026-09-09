// RUN: fcc compile --stage ir -o - %s | filecheck %s
// RUN: fcc compile --stage ir -o - %s | tir interp -f count --args=4 | filecheck --check-prefix=FOUR %s
// RUN: fcc compile --stage ir -o - %s | tir interp -f count --args=4 | filecheck --check-prefix=FOUR %s
// RUN: fcc compile --stage ir -o - %s | tir interp -f count --args=0 | filecheck --check-prefix=ZERO %s
// RUN: fcc compile --stage ir -o - %s | tir interp -f count --args=0 | filecheck --check-prefix=ZERO %s
// RUN: fcc compile --stage ir -O2 -o - %s | filecheck --check-prefix=OPT %s
// RUN: fcc compile --stage ir -O2 -o - %s | tir interp -f count --args=4 | filecheck --check-prefix=FOUR %s
// RUN: fcc compile --stage ir -O2 -o - %s | tir interp -f count --args=0 | filecheck --check-prefix=ZERO %s

// The unordered pipeline: `raise-loops` then `restructure-nodes`. A counted
// `for` becomes `scf.for` with the counter as its only value port, written
// back to the slot the body reads, and each slot's chain threaded through the
// body off a dependency port of its own. Both forms compute the same sum, zero
// trips included.

int count(int n) {
    int i;
    int total = 0;
    for (i = 0; i < n; i = i + 1) {
        total = total + i;
    }
    return total;
}

// CHECK: %{{[0-9]+}} = func.func @count
// CHECK-NOT: scf.loop
// CHECK: %[[LB:[0-9]+]], %{{[0-9]+}} = ptr.load %[[SLOT:[0-9]+]] state(%{{[0-9]+}}) : !i32
// CHECK: %{{[0-9]+}}, %{{[0-9]+}} = ptr.load %{{[0-9]+}} state(%{{[0-9]+}}) : !i32
// CHECK: %[[FIN:[0-9]+]], %[[M:[0-9]+]], %{{[0-9]+}} = scf.for %[[IV:[0-9]+]] = %[[LB]] to %{{[0-9]+}} step %{{[0-9]+}} (%[[D:[0-9]+]] = %{{[0-9]+}}, %{{[0-9]+}} = %{{[0-9]+}}) {
// CHECK-NEXT: %{{[0-9]+}}, %{{[0-9]+}} = ptr.load %{{[0-9]+}} state(%{{[0-9]+}}) : !i32
// CHECK-NEXT: %{{[0-9]+}} = ptr.store %[[IV]], %[[SLOT]] state(%[[D]])
// CHECK: -> %{{[0-9]+}}, %{{[0-9]+}}
// CHECK-NEXT: }
// CHECK: %{{[0-9]+}} = ptr.store %[[FIN]], %[[SLOT]] state(%[[M]])
// CHECK: -> %{{[0-9]+}}, %{{[0-9]+}}
// CHECK-NEXT: }

// FOUR: i32 6
// ZERO: i32 0

// At -O2 the unordered pipeline promotes both slots and folds what the loop
// carries: no memory operation is left, and the sum is the loop's result.
// OPT: func.func @count
// OPT-NOT: ptr.
// OPT-NOT: state.join
// OPT: scf.for
// OPT-NOT: ptr.
// OPT-NOT: state.join
