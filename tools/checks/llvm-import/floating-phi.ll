; RUN: tir llvm-import %s | filecheck %s
; RUN: tir mc --march x86_64 --filetype obj %s -o /dev/null

define double @choose(i1 %condition, double %value) {
entry:
  br i1 %condition, label %merge, label %other
other:
  br label %merge
merge:
  %result = phi double [ %value, %entry ], [ -1.000000e+00, %other ]
  ret double %result
}

; CHECK-LABEL: func.func @choose
; CHECK: fp.constant
; CHECK: func.return
