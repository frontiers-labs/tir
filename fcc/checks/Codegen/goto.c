// RUN: fcc compile --stage ir -o - %S/../Inputs/codegen_goto.c | filecheck --implicit-check-not=br %s

// A function holding a label is emitted as a flat graph of blocks, which the
// `restructure-nodes` pass raises back to an unordered region before the IR
// is handed on: the backward `goto` becomes a `scf.loop` whose predicate is
// the gamma over the exit test, and nothing branches.

// CHECK: %{{[0-9]+}} = func.func @sum_to
// CHECK: %{{[0-9]+}}, %[[OUT:[0-9]+]], %{{[0-9]+}} = scf.loop (%{{[0-9]+}} = %{{[0-9]+}}, %{{[0-9]+}} = %{{[0-9]+}}, %{{[0-9]+}} = %{{[0-9]+}}) {
// CHECK: %{{[0-9]+}}, %{{[0-9]+}} = ptr.load %{{[0-9]+}} state(%{{[0-9]+}}) : !i32
// CHECK-NEXT: %{{[0-9]+}}, %[[E:[0-9]+]] = ptr.load %{{[0-9]+}} state(%{{[0-9]+}}) : !i32
// CHECK: %[[P:[0-9]+]], %[[D0:[0-9]+]], %[[D1:[0-9]+]] = scf.switch
// CHECK: -> %[[P]], %[[E]], %[[D0]], %[[D1]], %[[E]], %[[D0]], %[[D1]]
// CHECK: }
// CHECK: ptr.load %{{[0-9]+}} state(%[[OUT]])
// CHECK: -> %{{[0-9]+}}, %{{[0-9]+}}
