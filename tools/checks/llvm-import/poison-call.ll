; RUN: tir mc --march x86_64 --filetype obj %s llvm -o /tmp/tir-poison-call-lit.o
; RUN: cc /tmp/tir-poison-call-lit.o %S/Inputs/poison-call-harness.c -o /tmp/tir-poison-call-lit
; RUN: /tmp/tir-poison-call-lit

define i32 @ignore_argument(i32 %unused) {
  ret i32 7
}

define i32 @call_with_poison() {
  %result = call i32 @ignore_argument(i32 poison)
  ret i32 %result
}
