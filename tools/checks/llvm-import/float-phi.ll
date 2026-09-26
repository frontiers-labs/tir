; RUN: tir mc --march x86_64 --filetype obj %s llvm -o /tmp/tir-float-phi-lit.o
; RUN: cc /tmp/tir-float-phi-lit.o %S/Inputs/float-phi-harness.c -o /tmp/tir-float-phi-lit
; RUN: /tmp/tir-float-phi-lit

define double @choose_float(i1 %condition, double %input) {
entry:
  br i1 %condition, label %zero, label %provided
zero:
  br label %merge
provided:
  br label %merge
merge:
  %result = phi double [ 0.000000e+00, %zero ], [ %input, %provided ]
  ret double %result
}
