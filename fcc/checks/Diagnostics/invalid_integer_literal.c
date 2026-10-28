// RUN: not fcc compile --std c23 --stage ir -o - %s 2>&1 | filecheck %s

int file_scope = 09;
// CHECK: [E0401] Error: invalid integer literal '09'

int block_scope(void) { return 08; }
// CHECK: [E0401] Error: invalid integer literal '08'

unsigned _BitInt(08) bit_int_width;
// CHECK: [E0401] Error: invalid integer literal '08'
