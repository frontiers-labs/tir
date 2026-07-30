// RUN: fcc compile --stage ast -o - %s | filecheck %s

#include <stdint.h>

uintptr_t identity(uintptr_t value)
{
    return value;
}

// CHECK: Typedef "uintptr_t": UnsignedLong
// CHECK: Function "identity"
// CHECK: Param "value": Named(uintptr_t)
