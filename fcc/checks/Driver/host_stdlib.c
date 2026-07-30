// RUN: fcc compile --stage ast -o - %s | filecheck %s

#include <stdlib.h>

void *allocate(size_t size)
{
    return malloc(size);
}

// CHECK: Typedef "lldiv_t"
// CHECK: Function "allocate"
