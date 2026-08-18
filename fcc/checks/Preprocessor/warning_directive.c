// RUN: fcc cc -E %s 2>&1 | filecheck %s

// #warning emits its diagnostic without failing the compilation, and the
// preprocessed output still reaches stdout.

#warning heads up
int main(void) { return 0; }

// CHECK: int main(void) { return 0; }
// CHECK: [W0300] Warning: heads up
// CHECK: #warning directive encountered
