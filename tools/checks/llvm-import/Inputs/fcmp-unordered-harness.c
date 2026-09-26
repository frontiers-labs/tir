extern _Bool less_or_unordered(double left, double right);
extern _Bool greater_or_unordered(double left, double right);
extern _Bool less_equal_or_unordered(double left, double right);
extern _Bool greater_equal_or_unordered(double left, double right);

int main(void) {
    double nan = __builtin_nan("");
    if (!less_or_unordered(nan, 0) || !greater_or_unordered(0, nan))
        return 1;
    if (!less_or_unordered(-1, 0) || less_or_unordered(1, 0))
        return 2;
    if (!greater_or_unordered(1, 0) || greater_or_unordered(-1, 0))
        return 3;
    if (!less_equal_or_unordered(nan, 0) || !greater_equal_or_unordered(0, nan))
        return 4;
    if (!less_equal_or_unordered(0, 0) || less_equal_or_unordered(1, 0))
        return 5;
    if (!greater_equal_or_unordered(0, 0) || greater_equal_or_unordered(-1, 0))
        return 6;
    return 0;
}
