; RUN: tir llvm-import %s | tir opt --verify | filecheck %s

; CHECK: module {
; CHECK-NEXT: %{{[0-9]+}} = global @g align 1 bytes [0, 0, 0, 0]
; CHECK-NEXT: %{{[0-9]+}} = func.func @sum(%{{[0-9]+}}: !i32, %{{[0-9]+}}: !i32) -> !i32 {
; CHECK-NEXT: %{{[0-9]+}} = ptr.alloca {size = 4, align = 4} : !ptr.p
; CHECK-NEXT: %{{[0-9]+}} = addi %{{[0-9]+}}, %{{[0-9]+}} : !i32
; CHECK-NEXT: %{{[0-9]+}} = constant {value = 1} : !i32
; CHECK-NEXT: %{{[0-9]+}} = addi %{{[0-9]+}}, %{{[0-9]+}} : !i32
; CHECK-NEXT: func.return %{{[0-9]+}}
; CHECK-NEXT: }
; CHECK-NEXT: module_end
; CHECK-NEXT: }

target datalayout = "e"

declare i32 @ext(i32)

@g = global i32 0

define i32 @sum(i32 %a, i32 %b) {
  %p = alloca i32, align 4
  %t = add i32 %a, %b
  %r = add i32 %t, 1
  ret i32 %r
}

!0 = !{}
