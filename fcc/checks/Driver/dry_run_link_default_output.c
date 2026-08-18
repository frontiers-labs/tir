// RUN: fcc cc -### %s | filecheck %s

// Linking without -o defaults the executable to a.out.

// CHECK: "fcc" "-c" "-o" "{{.*}}dry_run_link_default_output{{.*}}.o" "{{.*}}dry_run_link_default_output.c"
// CHECK: "cc" "-o" "a.out" "{{.*}}dry_run_link_default_output{{.*}}.o"
