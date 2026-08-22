// RUN: fcc compile --march riscv32 --stage ir -o - %S/../Inputs/long_target.c | filecheck %s --check-prefix=ILP32
// RUN: fcc compile --march riscv64 --stage ir -o - %S/../Inputs/long_target.c | filecheck %s --check-prefix=LP64

// ILP32: %{{[0-9]+}} = func.func @identity(%{{.*}}: !i32) -> !i32
// LP64: %{{[0-9]+}} = func.func @identity(%{{.*}}: !i64) -> !i64
