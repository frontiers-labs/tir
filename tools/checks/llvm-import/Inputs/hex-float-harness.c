extern double divide_by_max_int(double value);
extern float multiply_by_hex(float value);

int main(void) {
    return divide_by_max_int(2147483647.0) != 1.0 ||
           multiply_by_hex(1.0f) != 0x1.6a09e6p-1f;
}
