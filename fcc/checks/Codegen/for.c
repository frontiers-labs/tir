// RUN: fcc compile --stage ir -o - %S/../Inputs/basic_for.c | filecheck %s

// A counted `for` reaches the mid-end as `scf.for2`, not as the general
// `scf.loop` a flattened loop restructures into: the comparison and the
// increment are the loop op's own bounds and step, so the comparison is not
// spelled again and the loop carries no predicate of its own.

// CHECK-NOT: scf.loop
// CHECK: %[[UB:[0-9]+]] = constant {value = 3} : !i32
// CHECK: %[[ST:[0-9]+]] = constant {value = 1} : !i32
// CHECK: %[[LB:[0-9]+]] | %{{[0-9]+}} = ptr.load
// CHECK: scf.for2 %{{[0-9]+}} = %[[LB]] to %[[UB]] step %[[ST]] (
// CHECK-NOT: cmpi
// CHECK-NOT: scf.loop
