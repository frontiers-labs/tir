// RUN: not fcc compile --stage ast -o - %s 2>&1 | filecheck %s

// CHECK: [E0002] Error: unexpected end of file
// CHECK: found end of input
// CHECK: a brace, parenthesis or statement is left unclosed

int main(void) { return 0;
