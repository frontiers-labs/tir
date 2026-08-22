// RUN: fcc compile --stage ir -o - %S/../Inputs/codegen_double_compound_assign.c | filecheck %s

// CHECK: %{{[0-9]+}} = func.func @update(%{{[0-9]+}}: !f64, %{{[0-9]+}}: !f64) -> !f64 {
// CHECK: addf
// CHECK: ptr.store
// CHECK: subf
// CHECK: ptr.store
// CHECK: mulf
// CHECK: ptr.store
// CHECK: divf
// CHECK: ptr.store
