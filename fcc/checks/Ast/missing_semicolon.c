// RUN: not fcc compile --stage ast -o - %s 2>&1 | filecheck %s

int main(void) { return 0 }
// CHECK: [E0001] Error: unexpected token
// CHECK: found '}'
// CHECK: check for a missing or misplaced token near here
