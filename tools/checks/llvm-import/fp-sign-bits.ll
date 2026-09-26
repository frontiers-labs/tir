; RUN: tir mc --march x86_64 --filetype obj %s llvm -o /tmp/tir-fp-sign-bits-lit.o
; RUN: cc /tmp/tir-fp-sign-bits-lit.o %S/Inputs/fp-sign-bits-harness.c -o /tmp/tir-fp-sign-bits-lit
; RUN: /tmp/tir-fp-sign-bits-lit

declare float @llvm.fabs.f32(float)

define float @negate_bits(float %value) {
  %result = fneg float %value
  ret float %result
}

define float @absolute_bits(float %value) {
  %result = call float @llvm.fabs.f32(float %value)
  ret float %result
}
