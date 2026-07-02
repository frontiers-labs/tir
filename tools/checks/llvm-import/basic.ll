; RUN: tir llvm-import %s | filecheck %s

; CHECK: module {
; CHECK: func @sum
; CHECK: addi
; CHECK: constant
; CHECK: addi
; CHECK: return
; CHECK: module_end

define i32 @sum(i32 %a, i32 %b) {
  %t = add i32 %a, %b
  %r = add i32 %t, 1
  ret i32 %r
}
