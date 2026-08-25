// RUN: fcc cc -S -march=x86_64 -mcpu=tiger-lake -o - %s | filecheck %s
// RUN: not fcc cc -S -march=x86_64 -mcpu=nehalem -o - %s 2>&1 | filecheck %s --check-prefix=UNKNOWN

// -mcpu names one of the target's machine models — the same set `tir sched
// --model` selects from, so the compiler and the instrument schedule against
// the same machine. A cpu the target has no model for is an error, not a
// silently discarded flag.

int one(void) { return 1; }

// CHECK: one:
// CHECK: mov eax, 1
// UNKNOWN: fcc: error: unknown x86-64 cpu 'nehalem' (expected 'generic' or one of: tiger-lake)
