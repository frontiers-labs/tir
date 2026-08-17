// RUN: fcc compile --stage ir -o - %S/../Inputs/codegen_cast.c | filecheck %s

// CHECK: func.func @truncate
// CHECK: trunci
// CHECK: extui
// CHECK: func.func @widen
// CHECK: extsi
// CHECK: func.func @widen_unsigned
// CHECK: extui
// CHECK-LABEL: func.func @null_pointer
// CHECK: ptr.null : !ptr.p
// CHECK-LABEL: func.func @negative_pointer
// CHECK: extsi
// CHECK-LABEL: func.func @clear
// CHECK: ptr.null : !ptr.p
