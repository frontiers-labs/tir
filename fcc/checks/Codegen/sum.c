// RUN: fcc compile --stage ir -o - %S/../Inputs/codegen_sum.c | filecheck %s

// A two-parameter function lowers to two stack slots, with each parameter
// stored then loaded before the addition. Neither slot's address leaves its
// own loads and stores, so the entry state splits into a chain per slot and
// nothing orders one slot against the other.

// CHECK: module {
// CHECK: %{{[0-9]+}} = func.func @sum(%{{[0-9]+}}: !i32, %{{[0-9]+}}: !i32) -> !i32 {
// CHECK-COUNT-2: ptr.alloca
// CHECK: state(%[[E:[0-9]+]]) = state.entry_state
// CHECK: state(%[[C0:[0-9]+]], %[[C1:[0-9]+]], %{{[0-9]+}}) = state.split state(%[[E]])
// CHECK: state(%[[S0:[0-9]+]]) = ptr.store %{{[0-9]+}}, %{{[0-9]+}} state(%[[C0]])
// CHECK: state(%[[S1:[0-9]+]]) = ptr.store %{{[0-9]+}}, %{{[0-9]+}} state(%[[C1]])
// CHECK: %[[A:[0-9]+]], state(%{{[0-9]+}}) = ptr.load %{{[0-9]+}} state(%[[S0]]) : !i32
// CHECK: %[[B:[0-9]+]], state(%{{[0-9]+}}) = ptr.load %{{[0-9]+}} state(%[[S1]]) : !i32
// CHECK: addi %[[A]], %[[B]] : !i32
// CHECK: -> %{{[0-9]+}}, state(%{{[0-9]+}})
