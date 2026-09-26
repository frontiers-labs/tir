; RUN: tir mc --march x86_64 --filetype obj %s llvm -o /tmp/tir-hex-float-lit.o
; RUN: cc /tmp/tir-hex-float-lit.o %S/Inputs/hex-float-harness.c -o /tmp/tir-hex-float-lit
; RUN: /tmp/tir-hex-float-lit

define double @divide_by_max_int(double %value) {
  %result = fdiv double %value, 0x41DFFFFFFFC00000
  ret double %result
}

define float @multiply_by_hex(float %value) {
  %result = fmul float %value, 0x3FE6A09E60000000
  ret float %result
}
