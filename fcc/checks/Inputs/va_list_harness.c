extern int first_unnamed(int fixed, ...);
extern double first_double(int fixed, ...);
extern int sixth_unnamed(int fixed, ...);
extern double ninth_double(int fixed, ...);
extern int after_seven(int, int, int, int, int, int, int, ...);
struct four_longs { long values[4]; };
extern int after_large_struct(struct four_longs, ...);
struct two_longs { long values[2]; };
extern int after_spilled_struct(int, int, int, int, int, struct two_longs, ...);

int main(void) {
    if (first_unnamed(0, 42) != 42) return 1;
    if (first_double(0, 1.5) != 1.5) return 2;
    if (sixth_unnamed(0, 1, 2, 3, 4, 5, 6) != 6) return 3;
    if (ninth_double(0, 1., 2., 3., 4., 5., 6., 7., 8., 9.) != 9.) return 4;
    if (after_seven(1, 2, 3, 4, 5, 6, 7, 42) != 42) return 5;
    if (after_large_struct((struct four_longs){{1, 2, 3, 4}},
                           42, 11, 12, 13, 14, 15, 77) != 184) return 6;
    if (after_spilled_struct(1, 2, 3, 4, 5,
                             (struct two_longs){{6, 7}}, 42) != 42) return 7;
    return 0;
}
