unsigned long parse(unsigned long tokens, int shift) {
    unsigned long token = (tokens >> shift) & 15UL;
    int next = shift - 4;

    if (token <= 9UL) {
        return (token << 8) | (unsigned long)next;
    }

    {
        unsigned long left = parse(tokens, next);
        unsigned long right = parse(tokens, (int)(left & 255UL));
        unsigned long lhs = left >> 8;
        unsigned long rhs = right >> 8;
        unsigned long value = token == 10UL ? lhs + rhs : lhs * rhs;
        return (value << 8) | (right & 255UL);
    }
}

int main(void) {
    return (parse(0xA2B340UL, 20) >> 8) == 14UL ? 0 : 1;
}
