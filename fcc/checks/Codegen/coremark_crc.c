// RUN: fcc compile --stage asm --march x86_64 -o - %s | filecheck %s

unsigned short coremark_crc(unsigned char data, unsigned short crc)
{
    unsigned char i = 0;
    unsigned char x16 = 0;
    unsigned char carry = 0;

    for (i = 0; i < 8; i++)
    {
        x16 = (unsigned char)((data & 1) ^ ((unsigned char)crc & 1));
        data >>= 1;
        if (x16 == 1)
        {
            crc ^= 0x4002;
            carry = 1;
        }
        else
            carry = 0;
        crc >>= 1;
        if (carry)
            crc |= 0x8000;
        else
            crc &= 0x7fff;
    }
    return crc;
}

// CHECK: coremark_crc:
// CHECK: xor
// CHECK: or
// CHECK: and
