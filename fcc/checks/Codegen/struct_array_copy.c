// This file was generated with ./utils/scripts/update_checks.py. Do not modify CHECKs manually.

// RUN: fcc compile --march x86_64 --stage ir -o - %s | filecheck %s

struct record {
    char text[31];
};

void copy(struct record *destination, struct record *source) {
    *destination = *source;
}

// CHECK: #data_layout = {cache_line = 64, endianness = "little", stack_alignment = 128, types = {f32 = {abi = 32, size = 32}, f64 = {abi = 64, size = 64}, i1 = {abi = 8, size = 8}, i16 = {abi = 16, size = 16}, i32 = {abi = 32, size = 32}, i64 = {abi = 64, size = 64}, i8 = {abi = 8, size = 8}, p = {abi = 64, size = 64}}}
// CHECK-NEXT: #target_env = {arch = "x86_64", features = ["x86", "x86_64", "sse", "sse2"]}
// CHECK-EMPTY:
// CHECK: module {data_layout = #data_layout, target_env = #target_env} {
// CHECK-NEXT:   %2 = func.func @copy(%25: !ptr.p, %26: !ptr.p) {
// CHECK-NEXT:     %3 = ptr.alloca {size = 8, align = 8} : !ptr.p
// CHECK-NEXT:     %4 = ptr.alloca {size = 8, align = 8} : !ptr.p
// CHECK-NEXT:     %8 = constant {value = 0} : !i64
// CHECK-NEXT:     %10 = constant {value = 0} : !i64
// CHECK-NEXT:     %12 = constant {value = 31} : !i64
// CHECK-NEXT:     %27 = state.entry_state : !state<memory>
// CHECK-NEXT:     %28, %29, %30 = state.split state(%27)
// CHECK-NEXT:     %16 = ptr.store %25, %3 state(%28)
// CHECK-NEXT:     %17 = ptr.store %26, %4 state(%29)
// CHECK-NEXT:     %5, %18 = ptr.load %3 state(%16) : !ptr.p
// CHECK-NEXT:     %6, %19 = ptr.load %4 state(%17) : !ptr.p
// CHECK-NEXT:     %9 = ptr.ptradd %5, %8 : !ptr.p
// CHECK-NEXT:     %11 = ptr.ptradd %6, %10 : !ptr.p
// CHECK-NEXT:     %20 = ptr.memcpy %9, %11, %12 state(%30)
// CHECK-NEXT:     %7, %21 = ptr.load %5 state(%20) : !cir.struct<"record">
// CHECK-NEXT:     %31 = state.join state(%18, %19, %21)
// CHECK-NEXT:     -> %31
// CHECK-NEXT:   }
// CHECK-NEXT:   module_end
// CHECK-NEXT: }
