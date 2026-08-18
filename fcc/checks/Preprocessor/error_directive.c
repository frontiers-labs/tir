// RUN: not fcc cc -E %s 2>&1 | filecheck %s

#error broken
int main(void) { return 0; }

// CHECK: [E0300] Error: broken
// CHECK: #error directive encountered
// CHECK: C17 6.10.5
