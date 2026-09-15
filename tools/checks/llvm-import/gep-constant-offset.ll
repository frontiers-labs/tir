; RUN: tir llvm-import %s | tir opt --verify | filecheck %s

; CHECK: module {
; CHECK-NEXT: %{{[0-9]+}} = func.func @constant_gep() -> !i32 {
; CHECK-NEXT: %{{[0-9]+}} = ptr.alloca {size = 16, align = 4} : !ptr.p
; CHECK-NEXT: %{{[0-9]+}} = constant {value = 8} : !i64
; CHECK-NEXT: %{{[0-9]+}} = ptr.ptradd %{{[0-9]+}}, %{{[0-9]+}} : !ptr.p
; CHECK-NEXT: %{{[0-9]+}} = ptr.load %{{[0-9]+}} : !i32
; CHECK-NEXT: func.return %{{[0-9]+}}
; CHECK-NEXT: }
; CHECK: func.func @signed_narrow_gep() -> !i32 {
; CHECK-NEXT: %{{[0-9]+}} = ptr.alloca {size = 16, align = 4} : !ptr.p
; CHECK-NEXT: %{{[0-9]+}} = constant {value = -4} : !i64
; CHECK-NEXT: %{{[0-9]+}} = ptr.ptradd %{{[0-9]+}}, %{{[0-9]+}} : !ptr.p
; CHECK-NEXT: %{{[0-9]+}} = ptr.load %{{[0-9]+}} : !i32
; CHECK-NEXT: func.return %{{[0-9]+}}
; CHECK-NEXT: }
; CHECK: func.func @wrapped_gep() -> !i32 {
; CHECK-NEXT: %{{[0-9]+}} = ptr.alloca {size = 4, align = 4} : !ptr.p
; CHECK-NEXT: %{{[0-9]+}} = constant {value = 0} : !i64
; CHECK-NEXT: %{{[0-9]+}} = ptr.ptradd %{{[0-9]+}}, %{{[0-9]+}} : !ptr.p
; CHECK-NEXT: %{{[0-9]+}} = ptr.load %{{[0-9]+}} : !i32
; CHECK-NEXT: func.return %{{[0-9]+}}
; CHECK-NEXT: }
; CHECK: func.func @unit_scale_gep(%{{[0-9]+}}: !ptr.p, %{{[0-9]+}}: !i64) -> !ptr.p {
; CHECK-NEXT: %{{[0-9]+}} = ptr.ptradd %{{[0-9]+}}, %{{[0-9]+}} : !ptr.p
; CHECK-NEXT: func.return %{{[0-9]+}}
; CHECK-NEXT: }
; CHECK: func.func @mixed_gep(%{{[0-9]+}}: !ptr.p, %{{[0-9]+}}: !i64) -> !i32 {
; CHECK-NEXT: %{{[0-9]+}} = constant {value = 8} : !i64
; CHECK-NEXT: %{{[0-9]+}} = muli %{{[0-9]+}}, %{{[0-9]+}} : !i64
; CHECK-NEXT: %{{[0-9]+}} = constant {value = 4} : !i64
; CHECK-NEXT: %{{[0-9]+}} = addi %{{[0-9]+}}, %{{[0-9]+}} : !i64
; CHECK-NEXT: %{{[0-9]+}} = ptr.ptradd %{{[0-9]+}}, %{{[0-9]+}} : !ptr.p
; CHECK-NEXT: %{{[0-9]+}} = ptr.load %{{[0-9]+}} : !i32
; CHECK-NEXT: func.return %{{[0-9]+}}
; CHECK-NEXT: }
; CHECK: module_end
; CHECK-NEXT: }

target datalayout = "e-p:64:64"

define i32 @constant_gep() {
  %array = alloca [4 x i32], align 4
  %element = getelementptr [4 x i32], ptr %array, i64 0, i64 2
  %value = load i32, ptr %element, align 4
  ret i32 %value
}

define i32 @signed_narrow_gep() {
  %array = alloca [4 x i32], align 4
  %element = getelementptr [4 x i32], ptr %array, i64 0, i8 255
  %value = load i32, ptr %element, align 4
  ret i32 %value
}

define i32 @wrapped_gep() {
  %value = alloca i32, align 4
  %element = getelementptr i32, ptr %value, i64 -9223372036854775808
  %loaded = load i32, ptr %element, align 4
  ret i32 %loaded
}

define ptr @unit_scale_gep(ptr %p, i64 %index) {
  %element = getelementptr i8, ptr %p, i64 %index
  ret ptr %element
}

define i32 @mixed_gep(ptr %base, i64 %index) {
  %element = getelementptr [4 x { i32, i32 }], ptr %base, i64 0, i64 %index, i32 1
  %value = load i32, ptr %element, align 4
  ret i32 %value
}
