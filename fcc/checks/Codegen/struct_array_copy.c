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
// CHECK-NEXT:   %2 = func.func @copy(%19: !ptr.p, %20: !ptr.p) {
// CHECK-NEXT:     %3 = ptr.alloca {size = 8, align = 8} : !ptr.p
// CHECK-NEXT:     %4 = ptr.alloca {size = 8, align = 8} : !ptr.p
// CHECK-NEXT:     %7 = constant {value = 31} : !i64
// CHECK-NEXT:     %21 = state.entry_state : !state<memory>
// CHECK-NEXT:     %22, %23, %24 = state.split state(%21)
// CHECK-NEXT:     %11 = ptr.store %19, %3 state(%22)
// CHECK-NEXT:     %12 = ptr.store %20, %4 state(%23)
// CHECK-NEXT:     %5, %13 = ptr.load %3 state(%11) : !ptr.p
// CHECK-NEXT:     %6, %14 = ptr.load %4 state(%12) : !ptr.p
// CHECK-NEXT:     %15 = ptr.memcpy %5, %6, %7 state(%24)
// CHECK-NEXT:     %25 = state.join state(%13, %14, %15)
// CHECK-NEXT:     -> %25
// CHECK-NEXT:   }
// CHECK-NEXT:   module_end
// CHECK-NEXT: }
