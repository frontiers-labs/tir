// RUN: fcc compile --stage preprocess -o - %S/../Inputs/if_function_macro.c | filecheck %s

// CHECK: int selected;
