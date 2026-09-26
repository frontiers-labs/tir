typedef float pair __attribute__((vector_size(8)));

extern float vector_elements(pair value, float replacement);
extern pair build_vector(float real, float imaginary);
extern int vector_gep(const int *base, long row, long lane);

int main(void) {
    pair input = {2.0f, 3.0f};
    pair built = build_vector(5.0f, 7.0f);
    int values[8] = {0, 1, 2, 3, 4, 5, 6, 7};
    return vector_elements(input, 11.0f) != -9.0f ||
           built[0] != 5.0f || built[1] != 7.0f ||
           vector_gep(values, 1, 2) != 6;
}
