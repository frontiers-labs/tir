// RUN: fcc compile --stage ir -o - %S/../Inputs/codegen_switch.c | filecheck %s

// The case chain is nested gammas on one comparison each. Falling through
// from `case 1` into `case 2` is a gamma keyed on a value the chain yields,
// so the shared tail is emitted once.

// CHECK: %{{[0-9]+}} = func.func @classify
// CHECK: %[[C0:[0-9]+]] = cmpi {{.*}} {predicate = "eq"}
// CHECK: scf.switch %[[C0]]
// CHECK: %[[C1:[0-9]+]] = cmpi {{.*}} {predicate = "eq"}
// CHECK: %[[FALL:[0-9]+]] | %{{[0-9]+}} = scf.switch %[[C1]]
// CHECK: %[[C2:[0-9]+]] = cmpi {{.*}} {predicate = "eq"}
// CHECK: scf.switch %[[C2]]
// CHECK: constant {value = 9}
// CHECK: scf.switch %[[FALL]]
// CHECK: constant {value = 3}
// CHECK: addi
// CHECK: ptr.store
// CHECK: -> %{{[0-9]+}} | %{{[0-9]+}}
