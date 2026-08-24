// RUN: fcc compile --stage asm --march x86_64 -o - %s | filecheck %s

// The shape instruction selection is quadratic on: an `||` chain nests one
// assumption scope per test and a dense switch nests one more per arm, so each
// arm is covered under the whole stack of conditions proven above it. Selection
// must still spell every arm as the plain add it is, and must still use what the
// scope proves - the first two arms compare against the registers the chain
// above already pinned to 0 and 1 rather than against fresh immediates.

int classify(int value, int flag)
{
    if (flag == 0 || flag == 1 || flag == 2)
    {
        return 0;
    }

    switch (value)
    {
        case 0: return value + 100;
        case 1: return value + 101;
        case 2: return value + 102;
        case 3: return value + 103;
        case 4: return value + 104;
        case 5: return value + 105;
        default: return -1;
    }
}

// CHECK: classify:
// CHECK: cmp ecx, edi
// CHECK: add edi, 100
// CHECK: cmp eax, edi
// CHECK: add edi, 101
// CHECK: cmp edi, 2
// CHECK: add edi, 102
// CHECK: cmp edi, 3
// CHECK: add edi, 103
// CHECK: cmp edi, 5
// CHECK: add edi, 105
