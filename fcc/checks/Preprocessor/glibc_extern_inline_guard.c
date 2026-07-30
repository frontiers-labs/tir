// RUN: fcc compile --stage preprocess -o - %S/../Inputs/glibc_extern_inline_guard.c | filecheck %s

// CHECK: int optimize_undefined;
// CHECK: int float128_disabled;
// CHECK: int extern_inline_disabled;
