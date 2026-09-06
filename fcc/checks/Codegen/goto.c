// RUN: fcc compile --stage ir -o - %S/../Inputs/codegen_goto.c | filecheck --implicit-check-not=br %s

// A function holding a label is emitted as a flat graph of blocks, which the
// `restructure-nodes` pass raises back to an unordered region before the IR
// is handed on: the backward `goto` becomes a `scf.loop` whose predicate is
// the gamma over the exit test, and nothing branches.

// CHECK: %{{[0-9]+}} = func.func @sum_to
// CHECK: | %[[OUT:[0-9]+]] = scf.loop (| %{{[0-9]+}} = %{{[0-9]+}}) {
// CHECK: %[[P:[0-9]+]] | %[[D:[0-9]+]] = scf.switch2
// CHECK: -> %[[P]] | %[[D]], %[[D]]
// CHECK: }
// CHECK: ptr.load %{{[0-9]+}} | %[[OUT]]
// CHECK: -> %{{[0-9]+}} | %{{[0-9]+}}
