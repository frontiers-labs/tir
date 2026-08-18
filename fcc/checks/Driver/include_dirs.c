// RUN: fcc cc -E -I%S/Inputs/flagdir %s | filecheck %s
// RUN: fcc cc -E -I %S/Inputs/flagdir %s | filecheck %s

#include <flag_header.h>
int after;

// CHECK: int from_header{{;}}
// CHECK: int after{{;}}
