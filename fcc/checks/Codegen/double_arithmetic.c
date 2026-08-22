// RUN: fcc compile --stage ir -o - %S/../Inputs/codegen_double_arithmetic.c | filecheck %s

// CHECK: %{{[0-9]+}} = func.func @add(%{{[0-9]+}}: !f64, %{{[0-9]+}}: !f64) -> !f64 {
// CHECK: addf
// CHECK: %{{[0-9]+}} = func.func @subtract(%{{[0-9]+}}: !f64, %{{[0-9]+}}: !f64) -> !f64 {
// CHECK: subf
// CHECK: %{{[0-9]+}} = func.func @multiply(%{{[0-9]+}}: !f64, %{{[0-9]+}}: !f64) -> !f64 {
// CHECK: mulf
// CHECK: %{{[0-9]+}} = func.func @divide(%{{[0-9]+}}: !f64, %{{[0-9]+}}: !f64) -> !f64 {
// CHECK: divf
