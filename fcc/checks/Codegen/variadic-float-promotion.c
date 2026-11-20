// IR migration: instruction selection and backend lowering are unavailable.
// XFAIL: *
// RUN: fcc compile --stage ir -o - %S/../Inputs/codegen_variadic_float_promotion.c | filecheck %s
// RUN: fcc compile --march x86_64 --stage asm -o - %S/../Inputs/codegen_variadic_float_promotion.c | filecheck %s --check-prefix=ASM

// CHECK: func.func @print_float({{%[0-9]+}}: !f32) -> !i32
// CHECK: fp.convert {{%[0-9]+}} : !f64
// CHECK: func.call {{%[0-9]+}}({{%[0-9]+}}, {{%[0-9]+}} : !ptr.p, !f64)

// ASM-LABEL: print_float:
// ASM: cvtss2sd
// ASM: mov eax, 1
// ASM: call printf
