// RUN: fcc compile --stage ir -o - %S/../Inputs/do_continue.c | filecheck %s

// A `do`/`while` lowers to `cir.do` with the `body` region before the `cond`
// region. `continue` becomes a `cir.continue` naming the loop token.

// CHECK: cir.do %[[T:[0-9]+]] body {
// CHECK: scf.if
// CHECK: cir.continue %[[T]]
// CHECK: cond {
// CHECK: cmpi %{{[0-9]+}}, %{{[0-9]+}} {predicate = "slt"}
// CHECK: cir.condition
