// This file was generated with ./utils/scripts/update_checks.py. Do not modify CHECKs manually.

// RUN: fcc compile --march x86_64 --stage ir -o - %s | filecheck %s

struct record {
    char text[31];
};

void copy(struct record *destination, struct record *source) {
    *destination = *source;
}

// CHECK: #layout_spec = #layout.spec<endianness = "little", stack_align = 128, cache_line = 64, !i1 = {size = 8, abi = 8}, !i8 = {size = 8, abi = 8}, !i16 = {size = 16, abi = 16}, !i32 = {size = 32, abi = 32}, !i64 = {size = 64, abi = 64}, !f32 = {size = 32, abi = 32}, !f64 = {size = 64, abi = 64}, !ptr.p = {size = 64, abi = 64}, !builtin.i<W> = {size = !nat.align_to<W, !nat.min<!nat.max<!nat.pow2_ceil<W>, 8>, 64>>, abi = !nat.min<!nat.max<!nat.pow2_ceil<W>, 8>, 64>}, !builtin.f<15, 64> = {size = 128, abi = 128}>
// CHECK-NEXT: #target_env = {arch = "x86_64", features = ["x86", "x86_64", "sse", "sse2"]}
// CHECK-EMPTY:
// CHECK: module {layout.spec = #layout_spec, target_env = #target_env} {
// CHECK-NEXT:   type.def !record = !struct<{text: !array<31, !i8>}>
// CHECK-NEXT:   %2 = func.func @copy(%18: !ptr.p, %19: !ptr.p) {
// CHECK-NEXT:     %3 = ptr.alloca {size = 8, align = 8} : !ptr.p
// CHECK-NEXT:     %4 = ptr.alloca {size = 8, align = 8} : !ptr.p
// CHECK-NEXT:     %20 = state.entry_state : !state<memory>
// CHECK-NEXT:     %21, %22, %23 = state.split state(%20)
// CHECK-NEXT:     %10 = ptr.store %18, %3 state(%21)
// CHECK-NEXT:     %11 = ptr.store %19, %4 state(%22)
// CHECK-NEXT:     %5, %12 = ptr.load %3 state(%10) : !ptr.p
// CHECK-NEXT:     %6, %13 = ptr.load %4 state(%11) : !ptr.p
// CHECK-NEXT:     %25 = constant {value = 31} : !i64
// CHECK-NEXT:     %26 = ptr.memcpy %5, %6, %25 state(%23)
// CHECK-NEXT:     %24 = state.join state(%12, %13, %26)
// CHECK-NEXT:     -> %24
// CHECK-NEXT:   }
// CHECK-NEXT:   module_end
// CHECK-NEXT: }
