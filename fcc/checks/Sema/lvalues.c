// RUN: not fcc compile --std c23 --stage ir -o - %s 2>&1 | filecheck %s

// Assignment and increment require a modifiable lvalue.

int assign_to_rvalue(void) { 1 = 2; return 0; }
// CHECK: [E0403] Error: left operand is not a modifiable lvalue
// CHECK: N3220) 6.5.17.1p2

int increment_rvalue(void) { ++1; return 0; }
// CHECK: [E0403] Error: operand is not a modifiable lvalue
// CHECK: N3220) 6.5.4.1p1

int assign_to_const(void) { const int value = 1; value = 2; return value; }
// CHECK: [E0403] Error: left operand is not a modifiable lvalue
