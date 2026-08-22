// RUN: fcc compile --stage ir -o - %S/../Inputs/codegen_sum.c | filecheck %s

// A two-parameter function lowers to two stack slots, with each parameter
// stored then loaded before the addition.

// CHECK: module {
// CHECK: %{{[0-9]+}} = func.func @sum(%{{[0-9]+}}: !i32, %{{[0-9]+}}: !i32) -> !i32 {
// CHECK-COUNT-2: ptr.alloca
// CHECK: ptr.store
// CHECK: addi
// CHECK: func.return
