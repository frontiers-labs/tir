// RUN: fcc compile --stage asm --march x86_64 -o - %s | filecheck %s

int main(void) {
    int sum = 0;
    for (int i = 0; i < 32; i++)
        sum += i * 7;
    return sum;
}

// CHECK-LABEL: main:
