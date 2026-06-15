// RUN: fcc compile --stage ir -o - %S/../Inputs/while_count.c | filecheck %s

// A `while` lowers to `cir.while` with a `cond` region ending in
// `cir.condition` and a `body` region ending in `cir.yield`. The relational
// condition becomes a `cmpi`.

// CHECK: cir.while %{{[0-9]+}} cond {
// CHECK: cmpi %{{[0-9]+}}, %{{[0-9]+}} {predicate = "slt"}
// CHECK: cir.condition
// CHECK: body {
// CHECK: addi
// CHECK: cir.yield
