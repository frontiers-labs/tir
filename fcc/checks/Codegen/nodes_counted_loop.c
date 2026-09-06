// RUN: fcc compile --stage ir --nodes -o - %s | filecheck %s
// RUN: fcc compile --stage ir -o - %s | tir interp -f count --args=4 | filecheck --check-prefix=FOUR %s
// RUN: fcc compile --stage ir --nodes -o - %s | tir interp -f count --args=4 | filecheck --check-prefix=FOUR %s
// RUN: fcc compile --stage ir -o - %s | tir interp -f count --args=0 | filecheck --check-prefix=ZERO %s
// RUN: fcc compile --stage ir --nodes -o - %s | tir interp -f count --args=0 | filecheck --check-prefix=ZERO %s

// The unordered pipeline: `raise-loops` then `restructure-nodes`. A counted
// `for` becomes `scf.for2` with the counter as port 0, the carried slot copy
// after it, and the memory chain threaded through the body off a dependency
// port. Both forms compute the same sum, zero trips included.

int count(int n) {
    int i;
    int total = 0;
    for (i = 0; i < n; i = i + 1) {
        total = total + i;
    }
    return total;
}

// CHECK: %{{[0-9]+}} = func.func @count
// CHECK-NOT: scf.while
// CHECK: %[[LB:[0-9]+]] | %{{[0-9]+}} = ptr.load %[[SLOT:[0-9]+]] | %{{[0-9]+}} : !i32
// CHECK: %{{[0-9]+}}, %[[FIN:[0-9]+]] | %[[M:[0-9]+]] = scf.for2 %{{[0-9]+}} = %[[LB]] to %{{[0-9]+}} step %[[ST:[0-9]+]] (%[[IV:[0-9]+]] = %[[LB]] | %[[D:[0-9]+]] = %{{[0-9]+}}) {
// CHECK-NEXT: | %{{[0-9]+}} = ptr.store %[[IV]], %[[SLOT]] | %[[D]]
// CHECK: %[[NEXT:[0-9]+]] = addi %[[IV]], %[[ST]] : !i32
// CHECK: -> %[[NEXT]] | %{{[0-9]+}}
// CHECK-NEXT: }
// CHECK-NEXT: | %{{[0-9]+}} = ptr.store %[[FIN]], %[[SLOT]] | %[[M]]
// CHECK: -> %{{[0-9]+}} | %{{[0-9]+}}
// CHECK-NEXT: }

// FOUR: i32 6
// ZERO: i32 0
