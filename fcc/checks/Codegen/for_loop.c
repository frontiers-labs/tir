// RUN: fcc compile --stage ir -o - %S/../Inputs/for_loop.c | filecheck %s

// A `for` loop lowers to condition / body / step / join blocks. The loop test
// `i <= n` and the early-exit test `total > 100` each become a `cmpi`, and the
// step block branches back to the condition block.

// CHECK: func @sum_to(%{{[0-9]+}}: !i32) -> !i32 {
// CHECK: br ^bb{{[0-9]+}}
// CHECK: cmpi %{{[0-9]+}}, %{{[0-9]+}} {predicate = "sle"} : !i1
// CHECK: cond_br %{{[0-9]+}}, ^bb{{[0-9]+}}, ^bb{{[0-9]+}}
// CHECK: cmpi %{{[0-9]+}}, %{{[0-9]+}} {predicate = "sgt"} : !i1
// CHECK: cond_br %{{[0-9]+}}, ^bb{{[0-9]+}}, ^bb{{[0-9]+}}
// CHECK: return
