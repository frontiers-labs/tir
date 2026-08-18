// RUN: fcc cc -E -DFOO=1 -D BAR %s | filecheck %s --check-prefix=DEF
// RUN: fcc cc -E -DFOO=1 -UFOO %s | filecheck %s --check-prefix=UNDEF

// -D works attached and separate; -U removes a define. The patterns split the
// final token as a regex so these comments do not match themselves in the
// preprocessed output.

#ifdef FOO
int foo_defined;
#endif
#ifdef BAR
int bar_defined;
#endif
#ifndef FOO
int foo_missing;
#endif

// DEF: int foo_defined{{;}}
// DEF: int bar_defined{{;}}
// UNDEF: int foo_missing{{;}}
