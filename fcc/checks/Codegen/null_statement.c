// IR migration: instruction selection and backend lowering are unavailable.
// XFAIL: *
// RUN: fcc compile --stage ir -o - %S/../Inputs/null_statement.c | filecheck %s

// CHECK: %{{[0-9]+}} = func.func @main() -> !i32
// CHECK: -> %{{[0-9]+}}, %{{[0-9]+}}
