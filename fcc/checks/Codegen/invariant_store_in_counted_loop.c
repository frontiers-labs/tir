// RUN: fcc compile -O2 --stage asm --march x86_64 -o - %s | filecheck %s

// The loop writes one loop-invariant value to one loop-invariant address, so
// unrolling it leaves three stores of the same constant to the same address.
// The first two are dead: each is overwritten by the next with no read in
// between, so the mid-end keeps the last store only.

void invariant_store(int *p, int a)
{
    for (int i = 0; i < 3; i++) p[a & 3] = i >> 12;
}

// CHECK-LABEL: invariant_store:
// CHECK: and esi, 3
// CHECK: mov ecx, 0
// CHECK-NEXT: mov [rax], ecx
// CHECK-NEXT: ret
