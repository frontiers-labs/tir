// RUN: fcc cc -E %s %S/Inputs/other.c -o - | filecheck %s

// With -o - every input's output shares stdout.

int first;

// CHECK: int first{{;}}
// CHECK: int other(void) { return 0{{;}} }
