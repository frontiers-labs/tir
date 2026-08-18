int printf(const char *format, ...);

/* A flag assigned in both arms of an `if` and read after it, inside a loop whose
   `i++` step holds an identical constant. Promoting the flag out of memory puts
   one `1` inside the arm and one after the `if`, and the two must stay distinct
   values: the later one does not reach into the arm. */
static unsigned short crc_step(unsigned char data, unsigned short crc) {
    unsigned char i;
    unsigned char carry;

    carry = 0;
    for (i = 0; i < 8; i++) {
        if (data & 1) {
            crc = crc ^ 0x4002;
            carry = 1;
        } else {
            carry = 0;
        }
        crc = crc >> 1;
        if (carry) {
            crc = crc | 0x8000;
        } else {
            crc = crc & 0x7fff;
        }
    }
    return crc;
}

int main(void) {
    int i;
    unsigned short crc = 0;
    for (i = 0; i < 8; i++) {
        crc = crc_step((unsigned char)i, crc);
        printf("%u\n", (unsigned int)crc);
    }
    return 0;
}
