// RUN: fcc compile --stage ir -o - %S/../Inputs/codegen_float_binary32_literal.c | filecheck %s

// CHECK: func.func @decimal() -> !f32
// CHECK: %[[BITS:[0-9]+]] = constant {value = 1069547520} : !i32
// CHECK-NEXT: bitcast %[[BITS]] : !f32
