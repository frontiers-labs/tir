// RUN: fcc compile --stage ir -I %S/../Inputs -o - %S/../Inputs/hello_stdio.c | filecheck %s

// The string literal is a private δ in `.rodata`; the call takes the λ of the
// declaration of printf and the string's address as an ordinary operand, and
// hangs off the world chain the entry state splits out, since a call can touch
// memory of unknown provenance.

// CHECK: %[[STR:[0-9]+]] = global private @.L.str{{[0-9]+}} align 1 section ".rodata" bytes [104, 101, 108, 108, 111, 44, 32, 119, 111, 114, 108, 100, 10, 0]
// CHECK: %[[PRINTF:[0-9]+]] = func.declare @printf(!ptr.p, !varargs) -> !i32
// CHECK: %{{[0-9]+}} = func.func @main() -> !i32 {
// CHECK: %[[ENTRY:[0-9]+]] = state.entry_state : !state<memory>
// CHECK-NEXT: %[[LOCAL:[0-9]+]], %[[WORLD:[0-9]+]] = state.split state(%[[ENTRY]])
// CHECK-NEXT: %[[STORE:[0-9]+]] = ptr.store %{{[0-9]+}}, %{{[0-9]+}} state(%[[LOCAL]])
// CHECK-NEXT: %[[RESULT:[0-9]+]], %[[LOAD:[0-9]+]] = ptr.load %{{[0-9]+}} state(%[[STORE]]) : !i32
// CHECK-NEXT: %[[FP:[0-9]+]] = state.entry_state : !state<fp.env>
// CHECK-NEXT: %{{[0-9]+}}, %[[CALL:[0-9]+]], %[[CALL_FP:[0-9]+]] = func.call %[[PRINTF]](%[[STR]] : !ptr.p) -> !i32 state(%[[WORLD]], %[[FP]])
// CHECK-NEXT: %[[OUT:[0-9]+]] = state.join state(%[[LOAD]], %[[CALL]])
// CHECK-NEXT: -> %[[RESULT]], %[[OUT]], %[[CALL_FP]]
