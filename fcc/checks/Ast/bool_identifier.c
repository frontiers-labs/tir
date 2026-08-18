// RUN: fcc compile --std c17 --stage ast -o - %s | filecheck %s

// Before C23, bool is an ordinary identifier.

int bool(void);
// CHECK: Prototype "bool" -> Int
