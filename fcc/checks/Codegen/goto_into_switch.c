// RUN: fcc compile --stage ir -o - %S/../Inputs/codegen_goto_switch.c | filecheck --implicit-check-not=br %s

// A `goto` into a `switch` body is an edge into the middle of the comparison
// chain the switch lowers to, which `restructure-nodes` turns back into nested
// gammas: the gamma on `enter` yields a dispatch value, a later gamma keyed on
// it selects the arm jumped into, and that arm is emitted once.

// CHECK: %{{[0-9]+}} = func.func @dispatch
// CHECK: %[[ENTER:[0-9]+]] = cmpi {{.*}} {predicate = "ne"}
// CHECK: %[[INSIDE:[0-9]+]] | %{{[0-9]+}} = scf.switch2 %[[ENTER]]
// CHECK: constant {value = 1000}
// CHECK: constant {value = 10}
// CHECK: scf.switch2 %[[INSIDE]]
// CHECK: constant {value = 100}
// CHECK-NOT: constant {value = 100}
// CHECK: -> %{{[0-9]+}} | %{{[0-9]+}}
