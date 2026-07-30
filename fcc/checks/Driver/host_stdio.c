// RUN: fcc compile --stage ast -o - %s | filecheck %s

#include <stdio.h>

// CHECK: Prototype "printf"
int use_printf(void)
{
    return printf("fcc");
}
