// RUN: fcc compile --stage ir -o - %S/../Inputs/for_break.c | filecheck %s

// A `for` lowers to `cir.for` with cond/body/step regions. `break` becomes a
// `cir.break` naming the loop token, nested inside the `scf.if` it is guarded by.

// CHECK: cir.for %[[T:[0-9]+]] cond {
// CHECK: cir.condition
// CHECK: body {
// CHECK: scf.if
// CHECK: cir.break %[[T]]
// CHECK: step {
// CHECK: cir.yield
