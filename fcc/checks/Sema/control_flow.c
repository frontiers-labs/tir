// RUN: not fcc compile --std c23 --stage ir -o - %s 2>&1 | filecheck %s

// Statement placement and controlling-expression constraints.

int misplaced_break(void) { break; return 0; }
// CHECK: [E0503] Error: break statement is not inside a loop or switch
// CHECK: no enclosing loop or switch
// CHECK: N3220) 6.8.7.4p1

void sink(void);
int nonscalar_condition(void) { if (sink()) return 1; return 0; }
// CHECK: [E0500] Error: if condition must have scalar type
// CHECK: N3220) 6.8.5.2p1

int duplicate_case(int value) { switch (value) { case 1 + 1: return 1; case 2: return 2; } return 0; }
// CHECK: [E0502] Error: duplicate case value 2
// CHECK: previous case is here
// CHECK: N3220) 6.8.5.3p3

int missing_label(void) { goto missing; return 0; }
// CHECK: [E0204] Error: use of undeclared label 'missing'
// CHECK: N3220) 6.8.7.2p1
