// RUN: fcc compile --stage obj --march x86_64 -o /tmp/fcc-complex-abi-lit.o %s
// RUN: cc /tmp/fcc-complex-abi-lit.o %S/../Inputs/complex_abi_harness.c -o /tmp/fcc-complex-abi-lit
// RUN: /tmp/fcc-complex-abi-lit

float _Complex id_complex(float _Complex value) { return value; }
float _Complex load_complex(const float _Complex *value) { return *value; }
void store_complex(float _Complex *address, float _Complex value) { *address = value; }

struct fields { char prefix; float _Complex value; };

int complex_layout(void) {
    return sizeof(float _Complex) == 8 && sizeof(struct fields) == 12;
}
