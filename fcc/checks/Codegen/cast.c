// RUN: fcc compile --stage ir -o - %S/../Inputs/codegen_cast.c | filecheck %s

// CHECK: %{{[0-9]+}} = func.func @truncate
// CHECK: trunci
// CHECK: extui
// CHECK: %{{[0-9]+}} = func.func @widen
// CHECK: extsi
// CHECK: %{{[0-9]+}} = func.func @widen_unsigned
// CHECK: extui
// CHECK-LABEL: %{{[0-9]+}} = func.func @null_pointer
// CHECK: ptr.null : !ptr.p
// CHECK-LABEL: %{{[0-9]+}} = func.func @negative_pointer
// CHECK: extsi
// CHECK-LABEL: %{{[0-9]+}} = func.func @clear
// CHECK: ptr.null : !ptr.p
