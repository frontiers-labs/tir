// REQUIRES: linux, x86_64
// RUN: fcc compile --march x86_64 -O0 --stage obj -o - %s | python3 %S/../Inputs/run_object.py
// RUN: fcc compile --march x86_64 -O2 --stage obj -o - %s | python3 %S/../Inputs/run_object.py

void compare(int *result, int value) {
    *result = value == 65;
}

int main(void) {
    int result = -1;
    compare(&result, 65);
    if (result != 1)
        return 1;
    compare(&result, 64);
    return result != 0;
}
