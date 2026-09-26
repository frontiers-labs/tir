extern double choose_float(_Bool condition, double input);

int main(void) {
    return choose_float(1, 3.5) != 0.0 || choose_float(0, 3.5) != 3.5;
}
