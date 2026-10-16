// RUN: fcc compile --march riscv64 --stage ir -o - %S/../Inputs/integer_conversion.c | filecheck %s

// CHECK: %[[RIGHT:.*]] = ptr.load {{.*}} : !i32
// CHECK: %[[EXTENDED:.*]] = extsi %[[RIGHT:[0-9]+]] : !i64
// CHECK: addi {{.*}}, %[[EXTENDED:[0-9]+]] {nsw = true} : !i64
