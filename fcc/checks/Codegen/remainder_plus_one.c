// REQUIRES: linux, x86_64
// RUN: fcc compile --std gnu17 -O2 --stage obj --march x86_64 -o - %s | python3 %S/../Inputs/run_object.py

// The sum of `(x % 5) + 1` for `x` in `0..7` is 21. That added one is the same
// constant as the loop step. Folding it into the dividend instead yields
// `(x + 1) % 5`, and the sum is no longer 21.
int main(void) {
    unsigned x;
    unsigned sum = 0;
    for (x = 0; x < 8U; ++x)
        sum += (x % 5U) + 1U;
    return sum != 21U;
}
