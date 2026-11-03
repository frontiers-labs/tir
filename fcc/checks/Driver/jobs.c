// IR migration: instruction selection and backend lowering are unavailable.
// XFAIL: *
// REQUIRES: x86_64
// The worker count changes how many functions the mid-end optimises at once
// and nothing else: every count reads one epoch and commits in function order,
// so the assembly is the same one. Each function reads its sibling through a
// call the inliner takes, which is the cross-function read the epoch pins.
// RUN: fcc cc -O2 -S -o - %s | filecheck %s
// RUN: fcc cc -O2 -j1 -S -o - %s | filecheck %s
// RUN: fcc cc -O2 -j 8 -S -o - %s | filecheck %s
// RUN: fcc compile --stage asm --march x86_64 -O2 --jobs 4 -o - %s | filecheck %s
// RUN: not fcc cc -O2 -j0 -S -o - %s 2>&1 | filecheck %s --check-prefix=BAD

static int twice(int x) { return x + x; }
int a(int x) { return twice(x) + 1; }
int b(int x) { return twice(x) * 3; }
int c(int x) { return a(x) - b(x); }

// CHECK-LABEL: a:
// CHECK-NOT: call
// CHECK: ret
// CHECK-LABEL: b:
// CHECK-NOT: call
// CHECK: ret
// CHECK-LABEL: c:
// CHECK-NOT: call
// CHECK: ret

// BAD: invalid job count '0'
