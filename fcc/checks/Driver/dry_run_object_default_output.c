// RUN: fcc cc -### -c %s | filecheck %s

// Without -o, an object lands next to the working directory under the input's
// stem.

// CHECK: "fcc" "-c" "-o" "dry_run_object_default_output.o" "{{.*}}dry_run_object_default_output.c"
