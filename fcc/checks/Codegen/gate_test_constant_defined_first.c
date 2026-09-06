// RUN: fcc compile -O2 --stage asm --march x86_64 -o - %s | filecheck %s

// The zero both compares read is one register, and the first compare is the
// gate's own test. Destructuring sinks a literal to just ahead of its first
// reader; the gate's test is that reader, however the test is spelled, so the
// literal is defined before the branch and not in the merge block after it.

void f(int *r, int n)
{
    if (r[0] == 0) r[0] = 7;
    for (int i = 0; i < n; i++) if (r[i] == 0) r[i] = 1;
}

// CHECK-LABEL: f:
// CHECK: mov [[Z:e[a-z0-9]+]], 0
// CHECK-NEXT: cmp [[Z]],
// CHECK-NEXT: je
