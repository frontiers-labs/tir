// RUN: fcc compile -O2 --stage asm --march x86_64 -o - %s | filecheck %s

// Nightly differential-fuzz #399. A two-trip counted loop writes `p[i & 3]`.
// Unrolling spells those as stores to `p+0` and `p+4`. They are different
// extents of one object, so dead-store elimination must keep both. Treating
// them as one extent dropped the first store and left the caller's `p[0]`
// unchanged.

void distinct_offset_stores(int *p)
{
    for (int i = 0; i < 2; i++) {
        *(p + ((i) & 3)) = -(i);
        int b = (i % (8));
        p[(i) & 3] = (i / (((b & 7) + 1)));
    }
}

// CHECK-LABEL: distinct_offset_stores:
// CHECK: mov eax, 0
// CHECK-NEXT: mov [rdi + 0], eax
// CHECK-NEXT: mov [rdi + 4], eax
// CHECK-NEXT: ret
