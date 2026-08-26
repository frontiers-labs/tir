// RUN: fcc compile --stage ir --march x86_64 -o - %s | filecheck %s

// `restrict` on a pointer parameter is a no-alias guarantee about the memory
// the callee reaches through it, so it rides the λ as an argument attribute.
// A pointer without it names no argument.

void qualified(int *restrict y, const int *restrict x, int *plain)
{
    y[0] = x[0] + plain[0];
}

void unqualified(int *y, const int *x)
{
    y[0] = x[0];
}

// CHECK: func.func @qualified({{.*}}) noalias [0, 1] {
// CHECK: func.func @unqualified(
// CHECK-NOT: noalias
// CHECK-SAME: {
