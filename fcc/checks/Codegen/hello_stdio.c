// RUN: fcc compile --stage ir -I %S/../Inputs -o - %S/../Inputs/hello_stdio.c | filecheck %s

// CHECK: cir.global_string {sym_name = "[[STR:\.L\.str[0-9]+]]", value = "hello, world\n"}
// CHECK: func.declare @printf(!ptr.p, !cir.varargs) -> !i32
// CHECK: func.func @main() -> !i32 {
// CHECK: %{{[0-9]+}} = func.addr_of {sym_name = "[[STR]]"} : !ptr.p
// CHECK: func.call @printf(%{{[0-9]+}} : !ptr.p) -> !i32
// CHECK: func.return
