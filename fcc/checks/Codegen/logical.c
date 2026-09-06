// RUN: fcc compile --stage ir -o - %S/../Inputs/codegen_logical.c | filecheck %s

// `&&` evaluates its right operand only in the true arm of the switch and `||`
// only in the false arm; the other arm yields the short-circuit constant.

// CHECK: %{{[0-9]+}} = func.func @logical_and
// CHECK: scf.switch %{{[0-9]+}} args(
// CHECK-NEXT: -> %{{[0-9]+}} | %{{[0-9]+}}
// CHECK-NEXT: }
// CHECK-NEXT: (| %{{[0-9]+}}) {
// CHECK: addi
// CHECK: ->
// CHECK: %{{[0-9]+}} = func.func @logical_or
// CHECK: scf.switch %{{[0-9]+}} args(
// CHECK: addi
// CHECK: ->
// CHECK: }
// CHECK-NEXT: (| %{{[0-9]+}}) {
// CHECK-NEXT: -> %{{[0-9]+}} | %{{[0-9]+}}
