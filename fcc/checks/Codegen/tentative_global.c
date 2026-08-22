// RUN: fcc compile --stage ir -o - %s | filecheck %s

// A tentative definition and the definition that completes it name the same
// object: one symbol, carrying the initializer.

int x;
int x = 5;

// CHECK-COUNT-1: global @x align 4 bytes [5, 0, 0, 0]
// CHECK-NOT: @x
