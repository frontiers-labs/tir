// IR migration: instruction selection and backend lowering are unavailable.
// XFAIL: *
// RUN: fcc compile -O2 --stage ir -o - %s | filecheck %s
// RUN: fcc compile -O2 --march x86_64 --stage asm -o - %s | filecheck %s --check-prefix=ASM

// Inlining puts a callee's accesses of the caller's slot on a chain of the
// callee's own: the call took the merge of the caller's chains and its result
// is split back into them, so the body sits between the two and nothing relates
// the store it makes to the caller's own store of the slot. A merge of those
// two chains holds two different values for the slot and the chain has no one
// value to put on a port, so the slot stays memory. It is the seam that is
// conservative here, not the walk.

void sink(int);
static void set(char *p) { *p = 'x'; }

int probe(void)
{
    char x = 0;
    for (int i = 0; i < 2; i++)
    {
        set(&x);
        if (x != 'x')
            sink(i);
    }
    return x;
}

// CHECK-LABEL: func.func @probe
// CHECK: ptr.alloca
// CHECK: ptr.store
// CHECK: ptr.load

// ASM-LABEL: probe:
// ASM: ret
