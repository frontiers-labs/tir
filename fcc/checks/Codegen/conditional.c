// RUN: fcc compile --stage ir -o - %S/../Inputs/codegen_conditional.c | filecheck %s

// A conditional expression is an `scf.switch` whose arms each compute their
// operand and whose result is the value the expression takes.

// CHECK: %{{[0-9]+}} = func.func @conditional
// CHECK: %[[V:[0-9]+]] | %{{[0-9]+}} = scf.switch %{{[0-9]+}} args(
// CHECK: addi
// CHECK: ->
// CHECK: }
// CHECK-NEXT: (| %{{[0-9]+}}) {
// CHECK: addi
// CHECK: ->
// CHECK: ptr.store %[[V]]
