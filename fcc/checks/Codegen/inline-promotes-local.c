// RUN: fcc compile -O0 --march x86_64 --stage asm -o - %s | filecheck %s --check-prefix=O0
// RUN: fcc compile -O2 --march x86_64 --stage asm -o - %s | filecheck %s --check-prefix=O2

// The largest win inlining has in C: a local whose address only ever went to
// the callee. While the call stands the pointer escapes, so the slot is memory
// and stays memory. Inlining deletes the call, the address stops escaping, and
// `promote` — which runs after `inline` inside the round for this reason —
// carries the value on a port instead. Then there is nothing left but the
// constant.

static void init(int *p) { *p = 7; }

int f(void) { int s; init(&s); return s; }

// O0: call init
// O2-LABEL: f:
// O2-NEXT: mov eax, 7
// O2-NEXT: ret
