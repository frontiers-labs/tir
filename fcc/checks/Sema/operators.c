// RUN: not fcc compile --std c23 --stage ir -o - %s 2>&1 | filecheck %s

// Operator operand constraints.

void sink(void);
int add_void_operand(void) { return sink() + 1; }
// CHECK: [E0402] Error: operator '+' requires arithmetic operands
// CHECK: N3220) 6.5.7p2

int remainder_of_floats(float left, float right) { return left % right; }
// CHECK: [E0402] Error: operator '%' requires integer operands
// CHECK: N3220) 6.5.6p2

int conditional_void_condition(void) { return sink() ? 1 : 2; }
// CHECK: [E0402] Error: conditional operator requires a scalar condition
// CHECK: N3220) 6.5.16p2
