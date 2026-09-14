// REQUIRES: linux, x86_64
// RUN: fcc compile --std gnu17 --stage obj --march x86_64 -O0 -o - %s | python3 %S/../Inputs/run_object.py
// RUN: fcc compile --std gnu17 --stage obj --march x86_64 -O2 -o - %s | python3 %S/../Inputs/run_object.py

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
