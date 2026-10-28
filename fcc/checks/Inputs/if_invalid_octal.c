#define CAT(a, b) a##b
#define N 08
#if 08
int wrong_literal;
#endif
#if N
int wrong_macro;
#endif
#if CAT(0, 8)
int wrong_paste;
#endif
#if 010 == 8
int octal;
#endif
