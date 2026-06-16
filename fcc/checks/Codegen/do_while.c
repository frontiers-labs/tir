// RUN: fcc compile --stage ir -o - %S/../Inputs/do_while.c | filecheck %s

// A `do`/`while` loop runs its body before testing: control enters the body
// block directly, the `continue` inside the `if (i == 3)` jumps to the condition
// block, and the `while (i < n)` test branches back to the body.

// CHECK: func @countdown(%{{[0-9]+}}: !i32) -> !i32 {
// CHECK: br ^bb{{[0-9]+}}
// CHECK: cmpi %{{[0-9]+}}, %{{[0-9]+}} {predicate = "eq"} : !i1
// CHECK: cond_br %{{[0-9]+}}, ^bb{{[0-9]+}}, ^bb{{[0-9]+}}
// CHECK: cmpi %{{[0-9]+}}, %{{[0-9]+}} {predicate = "slt"} : !i1
// CHECK: cond_br %{{[0-9]+}}, ^bb{{[0-9]+}}, ^bb{{[0-9]+}}
// CHECK: return
