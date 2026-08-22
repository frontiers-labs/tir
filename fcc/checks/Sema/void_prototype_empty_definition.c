// RUN: fcc compile --std c17 --stage ir -o - %s | filecheck %s

// In C17 an empty-list definition is compatible with a (void) prototype.

int check(void);
int check() { return 1; }
// CHECK: %{{[0-9]+}} = func.func @check
