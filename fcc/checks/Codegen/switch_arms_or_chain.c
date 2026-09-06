// RUN: fcc compile -O2 --stage asm --march x86_64 -o - %s | filecheck %s

// The shape that used to make instruction selection quadratic: an `||` chain and
// a dense switch, one guarded arm per test. Every arm still arrives at selection
// folded to its literal, and the chain pins its first two comparisons to the
// registers holding 0 and 1. What each arm does with its literal is the gap:
// the return value lives in a stack slot whose first write sits inside an arm,
// so promote-nodes leaves the slot in memory and every arm spells its constant
// as a store to `[rsp]` that the exit reloads. The block-based promote kept the
// slot in `eax`, one `mov eax, N` per arm.

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
// CHECK: test edi, edi
// CHECK: cmp ecx, edi
// CHECK: cmp edi, 2
// CHECK: cmp edi, 3
// CHECK: cmp edi, 5
// CHECK: mov ecx, -1
// CHECK-NEXT: mov rax, rsp
// CHECK-NEXT: mov [rax], ecx
// CHECK: mov ecx, 110
// CHECK-NEXT: mov rax, rsp
// CHECK-NEXT: mov [rax], ecx
// CHECK: mov ecx, 106
// CHECK-NEXT: mov rax, rsp
// CHECK-NEXT: mov [rax], ecx
// CHECK: mov ecx, 100
// CHECK-NEXT: mov rax, rsp
// CHECK-NEXT: mov [rax], ecx
// CHECK: mov ecx, 0
// CHECK-NEXT: mov rax, rsp
// CHECK-NEXT: mov [rax], ecx
// CHECK: mov rax, rsp
// CHECK-NEXT: mov eax, [rax]
// CHECK-NEXT: add rsp, 16
// CHECK-NEXT: ret
