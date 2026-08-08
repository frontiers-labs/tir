// RUN: fcc compile --stage asm --march x86_64 -o - %s | filecheck %s

extern void observe(int);

int quotient_across_calls(int x, int y)
{
    if (x == 1) observe(x);
    if (y == 1) observe(y);
    return x / y;
}

// CHECK: quotient_across_calls:
// CHECK: ret
