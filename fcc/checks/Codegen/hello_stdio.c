// RUN: fcc compile --stage ir -I %S/../Inputs -o - %S/../Inputs/hello_stdio.c | filecheck %s

// CHECK: func.declare @printf(!ptr.p, !cir.varargs) -> !i32
// CHECK: func.func @main() -> !i32 {
// CHECK: cir.string {value = "hello, world\n"} : !ptr.p
// CHECK: func.call @printf(%{{[0-9]+}} : !ptr.p) -> !i32
// CHECK: func.return
