; RUN: tir llvm-import %s | tir opt --verify | filecheck %s

define i1 @below_or_unordered(double %value) {
  %result = fcmp ult double %value, 0x408F3FFFF0000000
  ret i1 %result
}

; CHECK-LABEL: func.func @below_or_unordered
; CHECK: constant {value = 4652007308572753920} : !i64
; CHECK: bitcast
; CHECK: fp.cmp {{.*}} {predicate = "oge"}
; CHECK: xori
; CHECK: func.return
