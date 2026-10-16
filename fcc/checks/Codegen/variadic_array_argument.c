// This file was generated with ./utils/scripts/update_checks.py. Do not modify CHECKs manually.

// RUN: fcc compile --march x86_64 --stage ir -o - %s | filecheck %s

char text[2];
void consume(int marker, ...);

void pass_array(void) {
    consume(0, text);
}

// CHECK: #layout_spec = #layout.spec<endianness = "little", stack_align = 128, cache_line = 64, !i1 = {size = 8, abi = 8}, !i8 = {size = 8, abi = 8}, !i16 = {size = 16, abi = 16}, !i32 = {size = 32, abi = 32}, !i64 = {size = 64, abi = 64}, !f32 = {size = 32, abi = 32}, !f64 = {size = 64, abi = 64}, !ptr.p = {size = 64, abi = 64}, !builtin.i<W> = {size = !nat.align_to<W, !nat.min<!nat.max<!nat.pow2_ceil<W>, 8>, 64>>, abi = !nat.min<!nat.max<!nat.pow2_ceil<W>, 8>, 64>}
// CHECK-NEXT: #target_env = {arch = "x86_64", features = ["x86", "x86_64", "sse", "sse2"]}
// CHECK-EMPTY:
// CHECK: module {layout.spec = #layout_spec, target_env = #target_env} {
// CHECK-NEXT:   %0 = global @text size 2 align 1
// CHECK-NEXT:   %1 = func.declare @consume(!i32, !cir.varargs) -> !unit
// CHECK-NEXT:   %4096 = func.func @pass_array() {
// CHECK-NEXT:     %4097 = constant {value = 0} : !i32
// CHECK-NEXT:     %4107 = state.entry_state : !state<memory>
// CHECK-NEXT:     %4108 = state.entry_state : !state<fp.env>
// CHECK-NEXT:     %4103, %4104 = func.call %1(%4097, %0 : !i32, !ptr.p) state(%4107, %4108)
// CHECK-NEXT:     -> %4103, %4104
// CHECK-NEXT:   }
// CHECK-NEXT:   module_end
// CHECK-NEXT: }
