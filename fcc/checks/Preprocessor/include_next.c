// RUN: fcc compile --stage preprocess -I %S/../Inputs/include_next/first -I %S/../Inputs/include_next/second -o - %s | filecheck %s

#include <nested/wrapper.h>

// CHECK: int from_first;
// CHECK: int from_second;
