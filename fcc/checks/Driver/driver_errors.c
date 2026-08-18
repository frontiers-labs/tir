// RUN: not fcc cc 2>&1 | filecheck %s --check-prefix=NOINPUT
// RUN: not fcc cc -### -c a.c b.c -o out.o 2>&1 | filecheck %s --check-prefix=MULTI

// NOINPUT: fcc: error: no input files
// MULTI: fcc: error: cannot specify -o with -c, -S or -E with multiple files
