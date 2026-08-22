// RUN: fcc compile --stage ir -o - %S/../Inputs/null_statement.c | filecheck %s

// CHECK: %{{[0-9]+}} = func.func @main() -> !i32
// CHECK: func.return %{{[0-9]+}}
