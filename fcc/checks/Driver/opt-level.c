// REQUIRES: x86_64
// The level picks a mid-end, and the mid-end is observable through inlining:
// the call to `twice` survives the level that runs no round and is inlined
// under every level that runs one.
// RUN: fcc cc -O0 -S -o - %s | filecheck %s --check-prefix=O0
// RUN: fcc cc -S -o - %s | filecheck %s --check-prefix=O0
// RUN: fcc cc -O1 -S -o - %s | filecheck %s --check-prefix=OPT
// RUN: fcc cc -O2 -S -o - %s | filecheck %s --check-prefix=OPT
// RUN: fcc cc -Os -Os -S -o - %s 2>&1 | filecheck %s --check-prefix=ALIAS

static int twice(int x) { return x + x; }
int call_twice(int x) { return twice(x); }

// A level with no round keeps the call.
// O0-LABEL: call_twice:
// O0: call twice

// OPT-LABEL: call_twice:
// OPT-NOT: call
// OPT: ret

// -Os aliases -O2, and says so once.
// ALIAS: fcc: warning: treating '-Os' as '-O2'
// ALIAS-NOT: fcc: warning: treating '-Os' as '-O2'
