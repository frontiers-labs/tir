// RUN: fcc compile --stage ir -o - %S/../Inputs/codegen_double_arithmetic.c | filecheck %s

// CHECK: func.func @add(%{{[0-9]+}}: !f64, %{{[0-9]+}}: !f64) -> !f64 {
// CHECK: addf
// CHECK: func.func @subtract(%{{[0-9]+}}: !f64, %{{[0-9]+}}: !f64) -> !f64 {
// CHECK: subf
// CHECK: func.func @multiply(%{{[0-9]+}}: !f64, %{{[0-9]+}}: !f64) -> !f64 {
// CHECK: mulf
// CHECK: func.func @divide(%{{[0-9]+}}: !f64, %{{[0-9]+}}: !f64) -> !f64 {
// CHECK: divf
