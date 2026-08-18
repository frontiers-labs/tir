// RUN: not fcc compile --std c17 --stage ast -o - %s 2>&1 | filecheck %s
// RUN: fcc compile --std c23 --stage ast -o - %s | filecheck %s --check-prefix=C23

int main(void) { return 0b1; }
// CHECK: [E0003] Error: binary integer literal is unavailable in C17
// C23: Function "main"
