// RUN: not fcc cc --bogus-flag %s 2>&1 | filecheck %s

// CHECK: fcc: error: unrecognized command-line option '--bogus-flag'
