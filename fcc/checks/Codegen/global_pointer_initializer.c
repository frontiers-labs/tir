// RUN: fcc compile --stage asm --march x86_64 -o - %s | filecheck %s

// A global pointer initializer is emitted as a symbolic directive, not bytes.

int target = 42;
int *pointer = &target;

// CHECK: pointer:
// CHECK-NEXT: .quad target
