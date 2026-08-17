// RUN: fcc compile --stage ir -o - %S/../Inputs/loop_control.c | filecheck %s

// A `continue` and a `break` are the two exits the body can take. Both become
// values the loop carries: the `scf.switch` inside the body runs the step only
// for the iteration that falls through, and the loop condition takes the rest.

// CHECK: func.func @stop_early
// CHECK: scf.while iter_args
// CHECK: scf.if
// CHECK: scf.switch %{{[0-9]+}} -> !i1 case 0 {
// CHECK: addi
// CHECK: ptr.store
// CHECK: scf.condition
