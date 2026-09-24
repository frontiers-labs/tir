; RUN: tir llvm-import %s | filecheck %s
; RUN: tir mc --march x86_64 --filetype obj %s -o /dev/null

define double @negate(double %value) {
  %result = fneg double %value
  ret double %result
}

; CHECK-LABEL: func.func @negate
; CHECK: bitcast
; CHECK: constant {value = -9223372036854775808} : !i64
; CHECK: xori
; CHECK: bitcast
; CHECK: func.return
