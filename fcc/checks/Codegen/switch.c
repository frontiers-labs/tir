// RUN: fcc compile --stage ir -o - %S/../Inputs/codegen_switch.c | filecheck %s

// The cases are one gamma on the controlling value. Falling through from
// `case 1` into `case 2` is a gamma keyed on a value the first one yields, so
// the shared tail is emitted once.

// CHECK: %{{[0-9]+}} = func.func @classify
// CHECK: %[[VALUE:[0-9]+]], %{{[0-9]+}} = ptr.load
// CHECK-NOT: cmpi
// CHECK: %[[FALL:[0-9]+]], %{{[0-9]+}} = scf.switch %[[VALUE]]
// CHECK: constant {value = 9}
// CHECK: scf.switch %[[FALL]]
// CHECK: constant {value = 3}
// CHECK: addi
// CHECK: ptr.store
// CHECK: -> %{{[0-9]+}}, %{{[0-9]+}}
