// REQUIRES: linux, x86_64
// RUN: fcc compile --march x86_64 -O0 --stage obj -o - %s | python3 %S/../Inputs/run_object.py
// RUN: fcc compile --march x86_64 -O2 --stage obj -o - %s | python3 %S/../Inputs/run_object.py

int compare(int value) {
    int result = value == 65;
    return result;
}

int compare_braced(int value) {
    int result[1] = {value == 65};
    return result[0];
}

int main(void) {
    return compare(65) != 1 || compare(64) != 0 ||
           compare_braced(65) != 1 || compare_braced(64) != 0;
}
