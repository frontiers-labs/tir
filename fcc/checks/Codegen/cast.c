// RUN: fcc compile --stage ir -o - %S/../Inputs/codegen_cast.c | filecheck %s

// CHECK: func @truncate
// CHECK: trunci
// CHECK: extui
// CHECK: func @widen
// CHECK: extsi
// CHECK: func @widen_unsigned
// CHECK: extui
// CHECK-LABEL: func @null_pointer
// CHECK: constant {value = 0} : !ptr.p
// CHECK-LABEL: func @negative_pointer
// CHECK: extsi
// CHECK-LABEL: func @clear
// CHECK: constant {value = 0} : !ptr.p
