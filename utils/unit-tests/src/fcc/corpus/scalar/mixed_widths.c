int main(void) {
    unsigned int state = 0x13579BDFU;
    signed char delta = -7;
    unsigned short salt = 50000;
    int index;

    for (index = 0; index < 16; ++index) {
        unsigned int amount = (unsigned int)index & 7U;
        state = (state ^ (unsigned int)salt) + (unsigned int)(delta * index);
        state = (state << amount) | (state >> ((32U - amount) & 31U));
        salt = (unsigned short)(salt + (unsigned short)(state & 255U));
        delta = (signed char)(delta + 3);
    }

    return (int)((state ^ (unsigned int)salt ^ (unsigned int)(unsigned char)delta) & 255U);
}
