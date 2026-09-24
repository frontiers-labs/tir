; RUN: tir llvm-import %s | tir opt --verify | filecheck %s

define i1 @below_or_unordered(double %value) {
  %result = fcmp ult double %value, 0x408F3FFFF0000000
  ret i1 %result
}

; CHECK-LABEL: func.func @below_or_unordered
; CHECK: fp.constant {bits = 4652007308572753920} : !f64
; CHECK: fp.cmp {{.*}} {predicate = "oge"}
; CHECK: xori
; CHECK: func.return
