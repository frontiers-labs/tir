// RUN: fcc compile --stage preprocess -o - %S/../Inputs/if_invalid_octal.c | filecheck %s

// CHECK-NOT: wrong
// CHECK: int octal;
