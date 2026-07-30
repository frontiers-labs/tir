// RUN: fcc compile --stage asm --march x86_64 -o - %s | filecheck %s

unsigned float_to_unsigned_local(double value)
{
    unsigned converted = (unsigned)value;
    if (converted == 0)
        converted = 1;
    return converted;
}

// CHECK: float_to_unsigned_local:
// CHECK: cvttsd2si
