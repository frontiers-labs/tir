// RUN: fcc compile --stage preprocess -o - %S/../Inputs/function_macro_recursion.c | filecheck %s

// CHECK: int result = FIRST(1);
