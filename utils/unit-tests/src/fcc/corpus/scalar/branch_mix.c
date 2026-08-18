int transform(int value, unsigned int round) {
    int divisor = (int)(round % 5U) + 1;
    int quotient = value / divisor;
    int remainder = value % divisor;

    if ((remainder < 0 && (round & 1U) != 0U) || quotient > 100) {
        quotient = -quotient;
    }

    switch (round & 3U) {
    case 0:
        value = quotient + remainder + 17;
        break;
    case 1:
        value = quotient - remainder - 29;
        break;
    case 2:
        value = quotient * 3 + remainder;
        break;
    default:
        value = quotient ^ (remainder - 41);
        break;
    }

    return value;
}

int main(void) {
    int value = -731;
    unsigned int round;

    for (round = 0; round < 31U; ++round) {
        value = transform(value, round);
        value %= 10000;
    }

    return value & 255;
}
