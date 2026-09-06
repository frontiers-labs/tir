// RUN: fcc compile --stage ir -o - %S/../Inputs/basic_if.c | filecheck %s

// An `if` with an `else` is a two-arm `scf.switch` on the comparison: arm 0
// is the `else`, arm 1 the `then`, and each arm stores on its own chain.

// CHECK: %[[C:[0-9]+]] = cmpi {{.*}} {predicate = "eq"} : !i1
// CHECK: scf.switch %[[C]] args(
// CHECK: ptr.store
// CHECK: ->
// CHECK: }
// CHECK-NEXT: (| %{{[0-9]+}}) {
// CHECK: ptr.store
// CHECK: ->
