// RUN: fcc compile --march riscv64 --stage ir -o - %S/../Inputs/compare_promotion.c | filecheck %s

// A comparison applies the usual arithmetic conversions to its operands, so a
// char is promoted to int before `cmpi` — the operands never differ in width.
// CHECK: %[[NARROW:.*]] = ptr.load {{.*}} : !i8
// CHECK: %[[PROMOTED:.*]] = extsi %[[NARROW]] : !i32
// CHECK: cmpi %[[PROMOTED]], {{.*}} {predicate = "ne"}

// Promotion also decides signedness: both operands become `int`, so an
// unsigned char compares signed.
// CHECK: func.func @below
// CHECK: cmpi {{.*}} {predicate = "slt"}

// `!` compares its operand against zero, so a comparison result feeding it is
// widened to its C type first rather than compared against an `int` zero.
// CHECK: func.func @negate
// CHECK: %[[INNER:.*]] = cmpi {{.*}} {predicate = "sge"}
// CHECK: %[[WIDENED:.*]] = extui %[[INNER]] : !i32
// CHECK: cmpi %[[WIDENED]], {{.*}} {predicate = "eq"}

// A narrow value used as a condition is promoted before its `!= 0` test, so no
// comparison is left at a sub-`int` width.
// CHECK: func.func @truth
// CHECK: %[[FLAG:.*]] = ptr.load {{.*}} : !i8
// CHECK: %[[WIDE:.*]] = extui %[[FLAG]] : !i32
// CHECK: cmpi %[[WIDE]], {{.*}} {predicate = "ne"}

// Comparison results have type `int`, so bitwise operands are widened before
// the operation.
// CHECK: func.func @both
// CHECK: %[[LEFT:.*]] = cmpi {{.*}} {predicate = "eq"}
// CHECK: %[[RIGHT:.*]] = cmpi {{.*}} {predicate = "eq"}
// CHECK: %[[LEFT_WIDE:.*]] = extui %[[LEFT]] : !i32
// CHECK: %[[RIGHT_WIDE:.*]] = extui %[[RIGHT]] : !i32
// CHECK: andi %[[LEFT_WIDE]], %[[RIGHT_WIDE]] : !i32
