// IR migration: instruction selection and backend lowering are unavailable.
// XFAIL: *
// RUN: env TIR_TIME_PASSES=1 fcc compile --stage asm --march x86_64 %s -o - 2>&1 | filecheck %s

// CHECK: tir-time: summary wall_ms={{[0-9]+(\.[0-9]+)?}} passes_ms={{[0-9]+(\.[0-9]+)?}}
// CHECK: tir-time: pass name={{[a-z0-9-]+}} total_ms={{[0-9]+(\.[0-9]+)?}} runs={{[1-9][0-9]*}}

int add(int a, int b) { return a + b; }
