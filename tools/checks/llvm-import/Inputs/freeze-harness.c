extern long long frozen_value(long long value);

int main(void) {
    return frozen_value(-123) != -123;
}
