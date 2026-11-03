// IR migration: instruction selection and backend lowering are unavailable.
// XFAIL: *
// RUN: env TIR_TIME_PASSES=1 fcc -O0 -c %s -o /dev/null 2>&1 | filecheck %s
// CHECK: fcc-time: frontend_ms={{[0-9]+\.[0-9]+}} passes_ms={{[0-9]+\.[0-9]+}} backend_ms={{[0-9]+\.[0-9]+}}

int add(int a, int b) { return a + b; }
