// REQUIRES: linux, x86_64
// RUN: fcc compile --std gnu17 --stage obj --march x86_64 -O0 -o %t.o %s
// RUN: timeout 30 cc %t.o -o %t
// RUN: timeout 30 %t
// RUN: fcc compile --std gnu17 --stage obj --march x86_64 -O2 -o %t.o %s
// RUN: timeout 30 cc %t.o -o %t
// RUN: timeout 30 %t

int output = -1;

void compare_to_int(int value) {
    output = value == 65;
}

int main(void) {
    compare_to_int(65);
    if (output != 1)
        return 1;
    compare_to_int(64);
    return output != 0;
}
