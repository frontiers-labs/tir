// This file was generated with ./utils/scripts/update_checks.py. Do not modify CHECKs manually.

// RUN: fcc compile --march x86_64 --stage ir -o - %s | filecheck %s

char text[2];
void consume(int marker, ...);

void pass_array(void) {
    consume(0, text);
}

// CHECK: #data_layout = {cache_line = 64, endianness = "little", stack_alignment = 128, types = {f32 = {abi = 32, size = 32}, f64 = {abi = 64, size = 64}, i1 = {abi = 8, size = 8}, i16 = {abi = 16, size = 16}, i32 = {abi = 32, size = 32}, i64 = {abi = 64, size = 64}, i8 = {abi = 8, size = 8}, p = {abi = 64, size = 64}}}
// CHECK-NEXT: #target_env = {arch = "x86_64", features = ["x86", "x86_64", "sse", "sse2"]}
// CHECK-EMPTY:
// CHECK: module {data_layout = #data_layout, target_env = #target_env} {
// CHECK-NEXT:   %0 = global @text size 2 align 1
// CHECK-NEXT:   %1 = func.declare @consume(!i32, !varargs) -> !unit
// CHECK-NEXT:   %2 = func.func @pass_array() {
// CHECK-NEXT:     %3 = constant {value = 0} : !i32
// CHECK-NEXT:     %13 = state.entry_state : !state<memory>
// CHECK-NEXT:     %14 = state.entry_state : !state<fp.env>
// CHECK-NEXT:     %9, %10 = func.call %1(%3, %0 : !i32, !ptr.p) state(%13, %14)
// CHECK-NEXT:     -> %9, %10
// CHECK-NEXT:   }
// CHECK-NEXT:   module_end
// CHECK-NEXT: }
