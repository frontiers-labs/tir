// REQUIRES: arm64
// RUN: fcc cc -S -o - %s | filecheck %s

// Without -march, compiling targets the host architecture.

int add(int a, int b) { return a + b; }

// CHECK: add:
// CHECK: add w0, w0, w1
// CHECK: ret
