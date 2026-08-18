// RUN: fcc compile --std c99 --stage preprocess -o - %s | filecheck %s --check-prefix=C99
// RUN: fcc compile --std c89 --stage preprocess -o - %s | filecheck %s --check-prefix=C89
// RUN: fcc cc -E -std=c99 %s | filecheck %s --check-prefix=C99

// C89 predefines no __STDC_VERSION__; later standards set their value, in the
// native CLI and in gcc mode alike.

#ifdef __STDC_VERSION__
long version = __STDC_VERSION__;
#else
int no_version;
#endif

// C99: long version = 199901L{{;}}
// C89: int no_version{{;}}
