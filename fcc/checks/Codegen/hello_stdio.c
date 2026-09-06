// RUN: fcc compile --stage ir -I %S/../Inputs -o - %S/../Inputs/hello_stdio.c | filecheck %s

// The string literal is a private δ in `.rodata`; the call takes the λ of the
// declaration of printf and the string's address as an ordinary operand, and
// hangs off the function's entry state.

// CHECK: %[[STR:[0-9]+]] = global private @.L.str{{[0-9]+}} align 1 section ".rodata" bytes [104, 101, 108, 108, 111, 44, 32, 119, 111, 114, 108, 100, 10, 0]
// CHECK: %[[PRINTF:[0-9]+]] = func.declare @printf(!ptr.p, !cir.varargs) -> !i32
// CHECK: %{{[0-9]+}} = func.func @main() -> !i32 {
// CHECK: | %[[ENTRY:[0-9]+]] = state.entry_state
// CHECK: func.call %[[PRINTF]](%[[STR]] : !ptr.p) -> !i32 | %[[ENTRY]]
// CHECK: -> %{{[0-9]+}} | %{{[0-9]+}}
