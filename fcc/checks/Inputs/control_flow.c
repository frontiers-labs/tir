int classify(int n) {
    int sign = 0;
    if (n < 0) {
        sign = -1;
    } else if (n == 0) {
        sign = 0;
    } else {
        sign = 1;
    }
    return sign;
}
