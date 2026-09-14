; RUN: tir llvm-import %s | filecheck %s

define i1 @choose(i1 %condition, i1 %if_true, i1 %if_false) {
  %result = select i1 %condition, i1 %if_true, i1 %if_false
  ret i1 %result
}

; CHECK-LABEL: func.func @choose
; CHECK-NOT: extsi
; CHECK: func.return
