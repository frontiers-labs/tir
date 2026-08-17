// RUN: fcc compile --stage ir -o - %S/../Inputs/early_return.c | filecheck %s

// An early return raises a flag instead of terminating a block. The loop
// condition, the loop step and everything after the loop are guarded by that
// flag, so the function stays structured and keeps a single `return`.

// CHECK: func.func @find
// CHECK: cir.for %{{[0-9]+}} cond {
// CHECK-NEXT: ptr.load %[[FLAG:[0-9]+]]
// CHECK: cir.if
// CHECK: cir.condition
// CHECK: body {
// CHECK: ptr.store %{{[0-9]+}}, %[[VALUE:[0-9]+]]
// CHECK-NEXT: %[[ONE:[0-9]+]] = constant {value = 1}
// CHECK-NEXT: ptr.store %[[ONE]], %[[FLAG]]
// CHECK-NOT: cir.break
// CHECK: step {
// CHECK-NEXT: ptr.load %[[FLAG]]
// CHECK: cir.if
// CHECK: ptr.load %[[FLAG]]
// CHECK: cir.if
// CHECK: %[[RESULT:[0-9]+]] = ptr.load %[[VALUE]]
// CHECK-NEXT: func.return %[[RESULT]]
