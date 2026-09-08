// RUN: fcc compile -O2 --stage asm --march x86_64 -o - %s | filecheck %s

// The shape that used to make instruction selection quadratic: an `||` chain and
// a dense switch, one guarded arm per test. Every arm still arrives at selection
// folded to its literal, and the chain pins its first two comparisons to the
// registers holding 0 and 1. The return value lives in a stack slot whose first
// write sits inside an arm; each arm leaves it holding a value of its own, so
// the probe answers every port the growth reads and the slot becomes the gate's
// own port — one `mov eax, N` per arm, and no stack frame at all.

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

// CHECK:     classify:
// CHECK:     test edi, edi
// CHECK:     cmp ecx, edi
// CHECK:     cmp edi, 2
// CHECK:     cmp edi, 3
// CHECK:     cmp edi, 5
// CHECK-NOT: rsp
// CHECK:     mov eax, -1
// CHECK:     mov eax, 110
// CHECK:     mov eax, 106
// CHECK:     mov eax, 100
// CHECK:     mov eax, 0
// CHECK:     ret
