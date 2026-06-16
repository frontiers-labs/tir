// RUN: fcc compile --stage ir -o - %S/../Inputs/control_flow.c | filecheck %s

// An `if`/`else if`/`else` chain lowers to a diamond of basic blocks: each test
// is a `cmpi` feeding a `cond_br`, and the branches rejoin through unconditional
// `br`s. The `-1` initializer becomes `0 - 1` via `subi`.

// CHECK: func @classify(%{{[0-9]+}}: !i32) -> !i32 {
// CHECK: cmpi %{{[0-9]+}}, %{{[0-9]+}} {predicate = "slt"} : !i1
// CHECK: cond_br %{{[0-9]+}}, ^bb{{[0-9]+}}, ^bb{{[0-9]+}}
// CHECK: subi
// CHECK: cmpi %{{[0-9]+}}, %{{[0-9]+}} {predicate = "eq"} : !i1
// CHECK: cond_br %{{[0-9]+}}, ^bb{{[0-9]+}}, ^bb{{[0-9]+}}
// CHECK: return
