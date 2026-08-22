// RUN: fcc compile --stage ir -o - %S/../Inputs/codegen_goto_switch.c | filecheck --implicit-check-not=br %s

// A `goto` into a `switch` body is an edge into the middle of the comparison
// chain the switch lowers to, which `restructure` turns back into nested
// conditionals: the arm jumped into is emitted once and still falls through.

// CHECK: %{{[0-9]+}} = func.func @dispatch
// CHECK: scf.if
// CHECK: func.return
