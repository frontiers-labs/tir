// RUN: fcc compile --stage ir -o - %S/../Inputs/break_unreachable.c | filecheck %s

// The body leaves on its first statement, so the loop is not a loop: what
// remains of it is a switch with two empty arms, and what follows the `break`
// is emitted nowhere.

// CHECK: %{{[0-9]+}} = func.func @stop
// CHECK-NOT: scf.loop
// CHECK: scf.switch2 %{{[0-9]+}} {
// CHECK-NEXT: ->
// CHECK-NEXT: }
// CHECK-NEXT: {
// CHECK-NEXT: ->
// CHECK-NEXT: }
