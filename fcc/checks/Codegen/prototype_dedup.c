// RUN: fcc compile --stage ir -o - %s | filecheck %s

// A prototype and the definition it announces are the same symbol, so only the
// definition reaches the module's symbol table.

int f(int);
int f(int x) { return x; }

// CHECK-NOT: func.declare
// CHECK: func.func @f(
// CHECK-NOT: func.declare
