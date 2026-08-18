int printf(const char *format, ...);

int main(void)
{
    unsigned short narrow = 0x8000;
    narrow ^= 0x4002;
    printf("%d\n", (int)narrow);

    unsigned short shifted = 0xF001;
    shifted <<= 4;
    printf("%d\n", (int)shifted);

    short widened = 1000;
    long factor = 1000000;
    widened *= factor;
    printf("%d\n", (int)widened);

    int quotient = 5;
    quotient /= 2.0;
    printf("%d\n", quotient);

    double accumulator = 1.5;
    accumulator += 3;
    printf("%.1f\n", accumulator);

    unsigned char byte = 200;
    byte >>= 3;
    printf("%d\n", (int)byte);

    return 0;
}
