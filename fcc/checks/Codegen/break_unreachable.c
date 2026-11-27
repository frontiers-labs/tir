// RUN: fcc compile --stage ir -o - %S/../Inputs/break_unreachable.c | filecheck --implicit-check-not=scf. %s

// The immediate break leaves no live conditional or loop. The load must
// observe the initial zero, without the assignment after the break.

// CHECK: %{{[0-9]+}} = func.func @stop
// CHECK: %[[I:[0-9]+]] = ptr.alloca
// CHECK: %[[ZERO:[0-9]+]] = constant {value = 0} : !i32
// CHECK: %[[INIT:[0-9]+]] = ptr.store %[[ZERO]], %[[I]]
// CHECK-NEXT: %[[VALUE:[0-9]+]], %{{[0-9]+}} = ptr.load %[[I]] state(%[[INIT]])
// CHECK-NEXT: %{{[0-9]+}} = ptr.store %[[VALUE]], %[[RESULT_SLOT:[0-9]+]]
// CHECK-NEXT: %[[RESULT:[0-9]+]], %{{[0-9]+}} = ptr.load %[[RESULT_SLOT]]
// CHECK: -> %[[RESULT]],
