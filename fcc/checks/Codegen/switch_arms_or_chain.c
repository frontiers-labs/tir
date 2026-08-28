// RUN: fcc compile --stage asm --march x86_64 -o - %s | filecheck %s

// The shape that used to make instruction selection quadratic: an `||` chain and
// a dense switch, one guarded arm per test. The facts are the mid-end's now — an
// arm that is entered only when `value` equals its case computes over that
// literal — so every arm arrives at selection already folded and is spelled as
// the single move it is. The chain above still pins its first two comparisons to
// the registers holding 0 and 1.

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
// CHECK: cmp eax, edi
// CHECK: mov eax, 100
// CHECK: cmp ecx, edi
// CHECK: mov eax, 102
// CHECK: cmp edi, 2
// CHECK: mov eax, 104
// CHECK: cmp edi, 3
// CHECK: mov eax, 106
// CHECK: cmp edi, 5
// CHECK: mov eax, 110
