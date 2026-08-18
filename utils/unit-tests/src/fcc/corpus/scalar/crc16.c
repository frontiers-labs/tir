typedef unsigned char ee_u8;
typedef unsigned short ee_u16;

int printf(const char *format, ...);

static ee_u16 crcu8(ee_u8 data, ee_u16 crc)
{
    ee_u8 i = 0, x16 = 0, carry = 0;

    for (i = 0; i < 8; i++)
    {
        x16 = (ee_u8)((data & 1) ^ ((ee_u8)crc & 1));
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

int main(void)
{
    ee_u16 crc = 0;
    int i;
    for (i = 0; i < 32; i++)
        crc = crcu8((ee_u8)(i * 7 + 1), crc);
    printf("%04x\n", (int)crc);
    return 0;
}
