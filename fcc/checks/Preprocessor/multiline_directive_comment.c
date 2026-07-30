// RUN: fcc compile --stage preprocess -o - %S/../Inputs/multiline_directive_comment.c | filecheck %s

// CHECK: int selected;
// CHECK-NOT: multi-line comment
// CHECK: int after;
