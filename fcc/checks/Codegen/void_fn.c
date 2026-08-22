// RUN: fcc compile --stage ir -o - %S/../Inputs/void_fn.c | filecheck %s

// A void function with no locals has no stack slots and just returns.

// CHECK: %{{[0-9]+}} = func.func @nop() {
// CHECK-NEXT: func.return
// CHECK-NEXT: }
// CHECK-NOT: ptr.alloca

// CHECK: %{{[0-9]+}} = func.func @implicit() {
// CHECK-NEXT: func.return
// CHECK-NEXT: }
