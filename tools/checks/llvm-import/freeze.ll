; RUN: tir mc --march x86_64 --filetype obj %s llvm -o /tmp/tir-freeze-lit.o
; RUN: cc /tmp/tir-freeze-lit.o %S/Inputs/freeze-harness.c -o /tmp/tir-freeze-lit
; RUN: /tmp/tir-freeze-lit

define i64 @frozen_value(i64 %value) {
  %frozen = freeze i64 %value
  ret i64 %frozen
}
