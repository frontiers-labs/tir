int fib(int value) {
    if (value < 2) {
        return value;
    }
    return fib(value - 1) + fib(value - 2);
}

int main(void) {
    return fib(10) == 55 ? 0 : 1;
}
