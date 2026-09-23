; RUN: tir llvm-import %s | tir opt --verify | filecheck %s

; LLVM disjoint OR has addition's value for every non-poison input.
; CHECK-LABEL: func.func @disjoint
; CHECK: addi
; CHECK-NOT: ori
; CHECK: func.return
 define i32 @disjoint(i32 %a, i32 %b) {
  %r = or disjoint i32 %a, %b
  ret i32 %r
}

; CHECK-LABEL: func.func @ordinary
; CHECK: ori
; CHECK: func.return
 define i32 @ordinary(i32 %a, i32 %b) {
  %r = or i32 %a, %b
  ret i32 %r
}
