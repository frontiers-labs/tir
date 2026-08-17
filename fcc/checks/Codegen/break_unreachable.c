// RUN: fcc compile --stage ir -o - %S/../Inputs/break_unreachable.c | filecheck %s

// The body leaves on its first statement, so the loop is not a loop and what
// follows the `break` is emitted nowhere.

// CHECK: func.func @stop
// CHECK-NOT: scf.while
// CHECK: scf.if %{{[0-9]+}} {
// CHECK-NEXT: scf.yield
// CHECK-NEXT: }
// CHECK-NEXT: else {
// CHECK-NEXT: scf.yield
