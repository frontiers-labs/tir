// REQUIRES: x86_64
// RUN: fcc cc -S -o - %s | filecheck %s

// Without -march, compiling targets the host architecture.

int add(int a, int b) { return a + b; }

// CHECK: add:
// CHECK: add eax, esi
// CHECK: ret
