// RUN: not fcc compile --std c17 --stage ir -o - %s 2>&1 | filecheck %s

// An empty-list definition is not compatible with a prototype that takes
// parameters.

int check(int value);
int check() { return 1; }
// CHECK: [E0202] Error: conflicting declarations for 'check'
