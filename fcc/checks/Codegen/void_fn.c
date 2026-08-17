// RUN: fcc compile --stage ir -o - %S/../Inputs/void_fn.c | filecheck %s

// A void function with no locals has no stack slots and just returns.

// CHECK: func.func @nop() {
// CHECK-NEXT: func.return
// CHECK-NEXT: }
// CHECK-NOT: ptr.alloca

// CHECK: func.func @implicit() {
// CHECK-NEXT: func.return
// CHECK-NEXT: }
