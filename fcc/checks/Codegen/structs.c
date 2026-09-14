// This file was generated with ./utils/scripts/update_checks.py. Do not modify CHECKs manually.

// RUN: fcc compile --march x86_64 --stage ir -o - %S/../Inputs/structs.c | filecheck %s

// CHECK: #data_layout = {cache_line = 64, endianness = "little", stack_alignment = 128, types = {f32 = {abi = 32, size = 32}, f64 = {abi = 64, size = 64}, i1 = {abi = 8, size = 8}, i16 = {abi = 16, size = 16}, i32 = {abi = 32, size = 32}, i64 = {abi = 64, size = 64}, i8 = {abi = 8, size = 8}, p = {abi = 64, size = 64}}}
// CHECK-NEXT: #target_env = {arch = "x86_64", features = ["x86", "x86_64", "sse", "sse2"]}
// CHECK-EMPTY:
// CHECK: module {data_layout = #data_layout, target_env = #target_env} {
// CHECK-NEXT:   %1 = func.func @read(%35: !ptr.p) -> !i32 {
// CHECK-NEXT:     %2 = ptr.alloca {size = 8, align = 8} : !ptr.p
// CHECK-NEXT:     %3 = ptr.alloca {size = 4, align = 4} : !ptr.p
// CHECK-NEXT:     %17 = constant {value = 4} : !i64
// CHECK-NEXT:     %36 = state.entry_state : !state<memory>
// CHECK-NEXT:     %37, %38, %39 = state.split state(%36)
// CHECK-NEXT:     %27 = ptr.store %35, %2 state(%37)
// CHECK-NEXT:     %4, %28 = ptr.load %2 state(%27) : !ptr.p
// CHECK-NEXT:     %18 = ptr.ptradd %4, %17 : !ptr.p
// CHECK-NEXT:     %6, %29 = ptr.load %18 state(%39) : !i32
// CHECK-NEXT:     %30 = ptr.store %6, %3 state(%38)
// CHECK-NEXT:     %7, %34 = ptr.load %3 state(%30) : !i32
// CHECK-NEXT:     %40 = state.join state(%28, %34, %29)
// CHECK-NEXT:     -> %7, %40
// CHECK-NEXT:   }
// CHECK-NEXT:   %8 = func.func @copy() -> !i32 {
// CHECK-NEXT:     %9 = ptr.alloca {size = 8, align = 4} : !ptr.p
// CHECK-NEXT:     %10 = ptr.alloca {size = 8, align = 4} : !ptr.p
// CHECK-NEXT:     %11 = ptr.alloca {size = 4, align = 4} : !ptr.p
// CHECK-NEXT:     %13 = constant {value = 37} : !i32
// CHECK-NEXT:     %19 = constant {value = 4} : !i64
// CHECK-NEXT:     %20 = ptr.ptradd %9, %19 : !ptr.p
// CHECK-NEXT:     %21 = constant {value = 4} : !i64
// CHECK-NEXT:     %22 = ptr.ptradd %10, %21 : !ptr.p
// CHECK-NEXT:     %23 = constant {value = 8} : !i64
// CHECK-NEXT:     %61 = state.entry_state : !state<memory>
// CHECK-NEXT:     %62, %63, %64, %65 = state.split state(%61)
// CHECK-NEXT:     %45 = state.join state(%62, %65)
// CHECK-NEXT:     %46 = ptr.store %13, %20 state(%45)
// CHECK-NEXT:     %47, %48 = state.split state(%46)
// CHECK-NEXT:     %49 = state.join state(%48, %47, %63)
// CHECK-NEXT:     %50 = ptr.memcpy %10, %9, %23 state(%49)
// CHECK-NEXT:     %51, %52, %53 = state.split state(%50)
// CHECK-NEXT:     %15, %54 = ptr.load %22 state(%53) : !i32
// CHECK-NEXT:     %55 = ptr.store %15, %11 state(%64)
// CHECK-NEXT:     %16, %60 = ptr.load %11 state(%55) : !i32
// CHECK-NEXT:     %66 = state.join state(%52, %54, %60, %51)
// CHECK-NEXT:     -> %16, %66
// CHECK-NEXT:   }
// CHECK-NEXT:   module_end
// CHECK-NEXT: }
