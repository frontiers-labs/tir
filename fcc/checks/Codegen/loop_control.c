// RUN: fcc compile --stage ir -o - %S/../Inputs/loop_control.c | filecheck %s

// A `continue` and a `break` are the two exits the body can take. Both become
// values the loop carries: the body's exit tag feeds a dispatch `scf.switch2`
// whose first arm runs the step only for the iterations that did not break,
// and the loop's predicate is the value that arm produces.

// CHECK: %{{[0-9]+}} = func.func @stop_early
// CHECK: scf.loop (
// CHECK: scf.switch2
// CHECK: %{{[0-9]+}}, %{{[0-9]+}}, %[[TAG:[0-9]+]] | %[[DEP:[0-9]+]] = scf.switch2
// CHECK: %[[PRED:[0-9]+]] | %{{[0-9]+}} = scf.switch2 %[[TAG]] args(| %[[DEP]]) (| %[[IN:[0-9]+]]) {
// CHECK: addi
// CHECK: ptr.store
// CHECK: -> %{{[0-9]+}}, %[[PRED]] | %{{[0-9]+}}
