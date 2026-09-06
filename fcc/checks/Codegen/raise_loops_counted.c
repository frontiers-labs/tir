// RUN: fcc compile --stage ir -o - %s | filecheck %s

// A counted `for` is raised to `scf.for`: the counter leaves its slot for a
// carried port, the bounds and step are read once before the loop, and the value
// the loop ends on is stored back so the code after it still reads a slot.
// Nothing branches — the loop is an operation, not a graph.

int count(int n) {
    int i;
    int total = 0;
    for (i = 0; i < n; i = i + 1) {
        total = total + i;
    }
    return total;
}

// CHECK-NOT: scf.loop
// CHECK: %[[LB:[0-9]+]] | %{{[0-9]+}} = ptr.load %[[SLOT:[0-9]+]] | %{{[0-9]+}} : !i32
// CHECK: %[[UB:[0-9]+]] | %{{[0-9]+}} = ptr.load %{{[0-9]+}} | %{{[0-9]+}} : !i32
// CHECK: %{{[0-9]+}}, %[[FINAL:[0-9]+]] | %[[OUT:[0-9]+]] = scf.for %[[IV:[0-9]+]] = %[[LB]] to %[[UB]] step %[[ST:[0-9]+]] (%[[ARG:[0-9]+]] = %[[LB]] | %[[DEP:[0-9]+]] = %{{[0-9]+}}) {
// CHECK-NEXT: | %{{[0-9]+}} = ptr.store %[[ARG]], %[[SLOT]] | %[[DEP]]
// CHECK: %[[NEXT:[0-9]+]] = addi %[[ARG]], %[[ST]] : !i32
// CHECK: -> %[[NEXT]] | %{{[0-9]+}}
// CHECK: ptr.store %[[FINAL]], %[[SLOT]] | %[[OUT]]
// CHECK-NOT: scf.loop
