// RUN: fcc compile --stage ir -o - %s | filecheck %s

// A counted `for` is raised to `scf.for`: the counter leaves its slot for the
// loop's own port, the bounds and step are read once before the loop, and the
// value the loop ends on is stored back so the code after it still reads a
// slot. Nothing branches — the loop is an operation, not a graph.

int count(int n) {
    int i;
    int total = 0;
    for (i = 0; i < n; i = i + 1) {
        total = total + i;
    }
    return total;
}

// CHECK-NOT: scf.loop
// CHECK: %[[LB:[0-9]+]], state(%{{[0-9]+}}) = ptr.load %[[SLOT:[0-9]+]] state(%{{[0-9]+}}) : !i32
// CHECK: %[[UB:[0-9]+]], state(%{{[0-9]+}}) = ptr.load %{{[0-9]+}} state(%{{[0-9]+}}) : !i32
// CHECK: %[[FINAL:[0-9]+]], state(%[[OUT:[0-9]+]], %{{[0-9]+}}) = scf.for %[[IV:[0-9]+]] = %[[LB]] to %[[UB]] step %{{[0-9]+}} (state(%[[DEP:[0-9]+]] = %{{[0-9]+}}, %{{[0-9]+}} = %{{[0-9]+}})) {
// CHECK-NEXT: %{{[0-9]+}}, state(%{{[0-9]+}}) = ptr.load %{{[0-9]+}} state(%{{[0-9]+}}) : !i32
// CHECK-NEXT: state(%{{[0-9]+}}) = ptr.store %[[IV]], %[[SLOT]] state(%[[DEP]])
// CHECK: -> state(%{{[0-9]+}}, %{{[0-9]+}})
// CHECK: ptr.store %[[FINAL]], %[[SLOT]] state(%[[OUT]])
// CHECK-NOT: scf.loop
