// RUN: fcc compile -O2 --stage asm --march x86_64 -o - %s | filecheck %s

// The loop writes one loop-invariant value to one loop-invariant address, so
// unrolling it leaves copies whose stored values all fold to the one constant.
// The address is computed once and the value is one `mov ecx, 0`, but all three
// stores stay: they sit in order on the function's one conservative memory
// chain, and nothing on that chain proves the first two dead before the third.
// Per-object chains let the block-based mid-end keep the last store only.

void invariant_store(int *p, int a)
{
    for (int i = 0; i < 3; i++) p[a & 3] = i >> 12;
}

// CHECK-LABEL: invariant_store:
// CHECK: and esi, 3
// CHECK: mov ecx, 0
// CHECK-NEXT: mov [rax], ecx
// CHECK-NEXT: mov [rax], ecx
// CHECK-NEXT: mov [rax], ecx
// CHECK-NEXT: ret
