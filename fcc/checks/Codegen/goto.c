// RUN: fcc compile --stage ir -o - %S/../Inputs/codegen_goto.c | filecheck --implicit-check-not=br %s

// A function holding a label is emitted as a flat graph of blocks, which the
// `restructure` pass raises back to structured control flow before the IR is
// handed on: the backward `goto` becomes a loop, and nothing branches.

// CHECK: %{{[0-9]+}} = func.func @sum_to
// CHECK: scf.while
// CHECK: scf.condition
// CHECK: func.return
