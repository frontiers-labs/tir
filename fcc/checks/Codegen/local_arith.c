// RUN: fcc compile --stage ir -o - %S/../Inputs/local_arith.c | filecheck %s

// Two parameters plus one local variable produce three stack slots, and the
// multiplication, constant and addition all appear.

// CHECK: %{{[0-9]+}} = func.func @f(%{{[0-9]+}}: !i32, %{{[0-9]+}}: !i32) -> !i32 {
// CHECK-COUNT-3: ptr.alloca
// CHECK: muli
// CHECK: constant {value = 1}
// CHECK: addi
// CHECK: func.return
