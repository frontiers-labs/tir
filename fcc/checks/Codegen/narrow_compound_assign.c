// RUN: fcc compile --stage asm --march x86_64 -o - %s | filecheck %s

void xor_u16(unsigned short *value)
{
    *value ^= 0x4002;
}

void xor_u16_in_branch(unsigned short *value, unsigned char *flag, int condition)
{
    if (condition)
    {
        *value ^= 0x4002;
        *flag = 1;
    }
    else
        *flag = 0;
}

unsigned short xor_u16_in_loop(unsigned short value)
{
    unsigned char i;
    for (i = 0; i < 8; i++)
    {
        if (i == 1)
            value ^= 0x4002;
    }
    return value;
}

// CHECK: xor_u16:
// CHECK: xor
// CHECK: xor_u16_in_branch:
// CHECK: xor
// CHECK: xor_u16_in_loop:
// CHECK: xor
