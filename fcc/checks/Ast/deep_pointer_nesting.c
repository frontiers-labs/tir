// A declarator nested 10000 pointers deep must not exhaust the stack or take
// time quadratic in its depth.
// RUN: fcc compile --stage ast -o - %S/../Inputs/deep_pointer_nesting.c | filecheck %s
// RUN: fcc compile --stage ir --march x86_64 -o - %S/../Inputs/deep_pointer_nesting.c | filecheck %s --check-prefix=IR

// CHECK: Global "deep" extern=false: Ptr(Ptr(Ptr({{.*}}Int{{\)+}}

// IR: %{{[0-9]+}} = global @deep size 8 align 8
