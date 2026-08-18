// A missing include is a hard error naming the requested path.
// RUN: not fcc cc -E %S/Inputs/missing_main.c 2>&1 | filecheck %s

// CHECK: [E0301] Error: 'does_not_exist.h' file not found
// CHECK: file not found
// CHECK: C17 6.10.2
