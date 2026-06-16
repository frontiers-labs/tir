// RUN: fcc compile --stage ir -o - %S/../Inputs/count_up.c | filecheck %s

// A `while` loop lowers to a condition block guarding the body: control first
// branches to the condition, the `i < n` test is a `cmpi` feeding a `cond_br`
// into the body or the join, and the body branches back to the condition.

// CHECK: func @count_up(%{{[0-9]+}}: !i32) -> !i32 {
// CHECK: br ^bb{{[0-9]+}}
// CHECK: cmpi %{{[0-9]+}}, %{{[0-9]+}} {predicate = "slt"} : !i1
// CHECK: cond_br %{{[0-9]+}}, ^bb{{[0-9]+}}, ^bb{{[0-9]+}}
// CHECK: br ^bb{{[0-9]+}}
// CHECK: return
