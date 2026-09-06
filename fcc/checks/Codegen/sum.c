// RUN: fcc compile --stage ir -o - %S/../Inputs/codegen_sum.c | filecheck %s

// A two-parameter function lowers to two stack slots, with each parameter
// stored then loaded before the addition.

// CHECK: module {
// CHECK: %{{[0-9]+}} = func.func @sum(%{{[0-9]+}}: !i32, %{{[0-9]+}}: !i32) -> !i32 {
// CHECK-COUNT-2: ptr.alloca
// CHECK: | %[[E:[0-9]+]] = state.entry_state
// CHECK: | %[[S0:[0-9]+]] = ptr.store %{{[0-9]+}}, %{{[0-9]+}} | %[[E]]
// CHECK: | %[[S1:[0-9]+]] = ptr.store %{{[0-9]+}}, %{{[0-9]+}} | %[[S0]]
// CHECK: %[[A:[0-9]+]] | %{{[0-9]+}} = ptr.load %{{[0-9]+}} | %[[S1]] : !i32
// CHECK: %[[B:[0-9]+]] | %{{[0-9]+}} = ptr.load %{{[0-9]+}} | %[[S1]] : !i32
// CHECK: addi %[[A]], %[[B]] : !i32
// CHECK: -> %{{[0-9]+}} | %{{[0-9]+}}
