// This file was generated with ./utils/scripts/update_checks.py. Do not modify CHECKs manually.

// RUN: fcc compile --march x86_64 --stage ir -o - %S/../Inputs/structs.c | filecheck %s

// CHECK: #layout_spec = #layout.spec<endianness = "little", stack_align = 128, cache_line = 64, !i1 = {size = 8, abi = 8}, !i8 = {size = 8, abi = 8}, !i16 = {size = 16, abi = 16}, !i32 = {size = 32, abi = 32}, !i64 = {size = 64, abi = 64}, !f32 = {size = 32, abi = 32}, !f64 = {size = 64, abi = 64}, !ptr.p = {size = 64, abi = 64}, !builtin.i<W> = {size = !nat.align_to<W, !nat.min<!nat.max<!nat.pow2_ceil<W>, 8>, 64>>, abi = !nat.min<!nat.max<!nat.pow2_ceil<W>, 8>, 64>}, !builtin.f<15, 64> = {size = 128, abi = 128}>
// CHECK-NEXT: #target_env = {arch = "x86_64", features = ["x86", "x86_64", "sse", "sse2"]}
// CHECK-EMPTY:
// CHECK: module {layout.spec = #layout_spec, target_env = #target_env} {
// CHECK-NEXT:   type.def !Pair = !struct<{tag: !i8, value: !i32}>
// CHECK-NEXT:   %1 = func.func @read(%19: !ptr.p) -> !i32 {
// CHECK-NEXT:     %2 = ptr.alloca {size = 8, align = 8} : !ptr.p
// CHECK-NEXT:     %3 = ptr.alloca {size = 4, align = 4} : !ptr.p
// CHECK-NEXT:     %20 = state.entry_state : !state<memory>
// CHECK-NEXT:     %21, %22, %23 = state.split state(%20)
// CHECK-NEXT:     %11 = ptr.store %19, %2 state(%21)
// CHECK-NEXT:     %4, %12 = ptr.load %2 state(%11) : !ptr.p
// CHECK-NEXT:     %25 = constant {value = 4} : !i64
// CHECK-NEXT:     %26 = ptr.ptradd %4, %25 : !ptr.p
// CHECK-NEXT:     %6, %13 = ptr.load %26 state(%23) : !i32
// CHECK-NEXT:     %14 = ptr.store %6, %3 state(%22)
// CHECK-NEXT:     %7, %18 = ptr.load %3 state(%14) : !i32
// CHECK-NEXT:     %24 = state.join state(%12, %18, %13)
// CHECK-NEXT:     -> %7, %24
// CHECK-NEXT:   }
// CHECK-NEXT:   %4096 = func.func @copy() -> !i32 {
// CHECK-NEXT:     %4097 = ptr.alloca {size = 8, align = 4} : !ptr.p
// CHECK-NEXT:     %4098 = ptr.alloca {size = 8, align = 4} : !ptr.p
// CHECK-NEXT:     %4099 = ptr.alloca {size = 4, align = 4} : !ptr.p
// CHECK-NEXT:     %4101 = constant {value = 37} : !i32
// CHECK-NEXT:     %4125 = state.entry_state : !state<memory>
// CHECK-NEXT:     %4126, %4127, %4128, %4129 = state.split state(%4125)
// CHECK-NEXT:     %4109 = state.join state(%4126, %4129)
// CHECK-NEXT:     %4131 = constant {value = 4} : !i64
// CHECK-NEXT:     %4132 = ptr.ptradd %4097, %4131 : !ptr.p
// CHECK-NEXT:     %4110 = ptr.store %4101, %4132 state(%4109)
// CHECK-NEXT:     %4111, %4112 = state.split state(%4110)
// CHECK-NEXT:     %4113 = state.join state(%4112, %4111, %4127)
// CHECK-NEXT:     %4133 = constant {value = 8} : !i64
// CHECK-NEXT:     %4134 = ptr.memcpy %4098, %4097, %4133 state(%4113)
// CHECK-NEXT:     %4115, %4116, %4117 = state.split state(%4134)
// CHECK-NEXT:     %4135 = constant {value = 4} : !i64
// CHECK-NEXT:     %4136 = ptr.ptradd %4098, %4135 : !ptr.p
// CHECK-NEXT:     %4103, %4118 = ptr.load %4136 state(%4117) : !i32
// CHECK-NEXT:     %4119 = ptr.store %4103, %4099 state(%4128)
// CHECK-NEXT:     %4104, %4124 = ptr.load %4099 state(%4119) : !i32
// CHECK-NEXT:     %4130 = state.join state(%4116, %4118, %4124, %4115)
// CHECK-NEXT:     -> %4104, %4130
// CHECK-NEXT:   }
// CHECK-NEXT:   module_end
// CHECK-NEXT: }
