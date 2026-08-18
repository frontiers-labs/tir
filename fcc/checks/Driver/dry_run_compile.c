// RUN: fcc cc -### -c %s -o out.o | filecheck %s
// A bare gcc flag routes fcc into gcc mode without the `cc` subcommand.
// RUN: fcc -### -c %s -o out.o | filecheck %s

// CHECK: "fcc" "-c" "-o" "out.o" "{{.*}}dry_run_compile.c"
