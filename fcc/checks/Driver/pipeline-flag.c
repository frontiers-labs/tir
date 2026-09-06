// RUN: fcc compile --stage asm --march x86_64 --pipeline func.func(verify-deps,instcombine-nodes) -o - %s | filecheck %s
// RUN: not fcc compile --stage asm --march x86_64 --pipeline func.func(no-such-pass) -o - %s 2>&1 | filecheck %s --check-prefix=BAD

int add(int a, int b)
{
    return a + b;
}

// CHECK: add:
// CHECK: ret

// BAD: unknown pass 'no-such-pass'
