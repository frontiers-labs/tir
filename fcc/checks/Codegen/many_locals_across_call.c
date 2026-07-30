// RUN: fcc compile --stage asm --march x86_64 -o - %s | filecheck %s

extern void observe(int *, int *, int *, int *, int *, int *, int *, int *);

int many_locals_across_call(int value)
{
    int a = value + 1;
    int b = value + 2;
    int c = value + 3;
    int d = value + 4;
    int e = value + 5;
    int f = value + 6;
    int g = value + 7;
    int h = value + 8;
    observe(&a, &b, &c, &d, &e, &f, &g, &h);
    return a + b + c + d + e + f + g + h;
}

// CHECK: many_locals_across_call:
// CHECK: call observe
