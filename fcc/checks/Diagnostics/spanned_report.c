// RUN: not fcc compile --stage ast -o - %s 2>&1 | filecheck %s

// A spanned diagnostic renders a full ariadne frame: the code and message,
// the source position, the offending line, the help and the standard note.

int main(void) { return 0 }
// CHECK: [E0001] Error: unexpected token
// CHECK: spanned_report.c:6:27 ]
// CHECK: int main(void) { return 0 }
// CHECK: found '}'
// CHECK: Help: check for a missing or misplaced token near here
// CHECK: Note: C17 6.9
