int printf(const char *format, ...);

/* Repeatedly clears the largest element of an array, remembering its
   position. The inner loop's taken branch (a new maximum) reads the counter
   the loop header carries, while the fallthrough backedge already redefines
   that counter for the next iteration, so the two must not share a register
   with the address materialized between them. */
int x[10] = {3, 1, 4, 1, 5, 9, 2, 6, 5, 3};

int main(void) {
    int rounds = 0;
    for (;;) {
        int i, mi = -1, max = 0;
        for (i = 0; i < 10; i++) {
            if (x[i] > max) {
                max = x[i];
                mi = i;
            }
        }
        if (max == 0) {
            break;
        }
        printf("%d %d\n", mi, max);
        x[mi] = 0;
        rounds++;
    }
    return rounds;
}
