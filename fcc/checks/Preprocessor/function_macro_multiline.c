// RUN: fcc compile --stage preprocess -o - %S/../Inputs/function_macro_multiline.c | filecheck %s

// CHECK: int result = ((2) + (3));
