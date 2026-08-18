// RUN: not fcc compile --std c23 --stage ir -o - %s 2>&1 | filecheck %s

// In C23 an empty parameter list is a (void) prototype, so extra arguments are
// a constraint violation.

int legacy();
int caller(void) { return legacy(1); }
// CHECK: [E0406] Error: function 'legacy' expects 0 arguments but 1 was provided
