// RUN: fcc compile --march riscv --stage ir -o - %s | filecheck %s

// The C pointer width is read from the target's declared data layout, so any
// spelling the target accepts works without fcc keeping its own march table.

unsigned long pointer_size(void) { return sizeof(void *); }

// CHECK: p = {abi = 64, size = 64}
// CHECK: constant {value = 8}
