// RUN: fcc cc -### -c %s -oattached.o | filecheck %s
// RUN: fcc cc -### -c -MF dep.d %s -o consumed.o | filecheck %s --check-prefix=MF
// RUN: not fcc cc -c %s -o 2>&1 | filecheck %s --check-prefix=MISSING
// RUN: not fcc cc foo.txt 2>&1 | filecheck %s --check-prefix=POSITIONAL
// RUN: not fcc cc --help 2>&1 | filecheck %s --check-prefix=HELP
// RUN: not fcc cc -std=bogus %s 2>&1 | filecheck %s --check-prefix=STD

// CHECK: "fcc" "-c" "-o" "attached.o" "{{.*}}gcc_flags.c"

// -MF takes a value, which must not be mistaken for an input file.
// MF: "fcc" "-c" "-o" "consumed.o" "{{.*}}gcc_flags.c"
// MF-NOT: dep.d

// MISSING: fcc: error: missing argument to '-o'
// POSITIONAL: fcc: error: unrecognized command-line option 'foo.txt'
// HELP: fcc: error: unrecognized command-line option '--help'
// STD: fcc: error: unsupported C language standard 'bogus'
