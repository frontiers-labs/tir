// RUN: fcc compile --stage ir -o - %S/../Inputs/null_statement.c | filecheck %s

// CHECK: func.func @main() -> !i32
// CHECK: func.return %{{[0-9]+}}
