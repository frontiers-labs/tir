; RUN: tir mc --march x86_64 --filetype obj %s llvm -o /tmp/tir-fcmp-unordered-lit.o
; RUN: cc /tmp/tir-fcmp-unordered-lit.o %S/Inputs/fcmp-unordered-harness.c -o /tmp/tir-fcmp-unordered-lit
; RUN: /tmp/tir-fcmp-unordered-lit

define i1 @less_or_unordered(double %left, double %right) {
  %result = fcmp ult double %left, %right
  ret i1 %result
}

define i1 @greater_or_unordered(double %left, double %right) {
  %result = fcmp ugt double %left, %right
  ret i1 %result
}

define i1 @less_equal_or_unordered(double %left, double %right) {
  %result = fcmp ule double %left, %right
  ret i1 %result
}

define i1 @greater_equal_or_unordered(double %left, double %right) {
  %result = fcmp uge double %left, %right
  ret i1 %result
}
