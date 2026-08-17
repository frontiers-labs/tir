// RUN: fcc compile --stage ir -o - %S/../Inputs/codegen_compound_assign.c | filecheck %s

// CHECK: func.func @compound_assign
// CHECK: addi
// CHECK: muli
// CHECK: subi
// CHECK: shli
// CHECK: shrsi
// CHECK: andi
// CHECK: xori
// CHECK: ori
