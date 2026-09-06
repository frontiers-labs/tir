// RUN: fcc compile -O2 --stage asm --march x86_64 -o - %s | filecheck %s

// The address of a global is the function's result: the mid-end forwards it
// through the slot the frontend spilled it to, so the global's value sits in
// the body's result list, where it is materialized like any other use.

int foo;
int *bar(void) { return &foo; }

// CHECK-LABEL: bar:
// CHECK-NEXT: lea rax, [rip + foo]
// CHECK-NEXT: ret
