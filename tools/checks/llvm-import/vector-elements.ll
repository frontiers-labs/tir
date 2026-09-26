; RUN: tir mc --march x86_64 --filetype obj %s llvm -o /tmp/tir-vector-elements-lit.o
; RUN: cc /tmp/tir-vector-elements-lit.o %S/Inputs/vector-elements-harness.c -o /tmp/tir-vector-elements-lit
; RUN: /tmp/tir-vector-elements-lit

define float @vector_elements(<2 x float> %value, float %replacement) {
  %squared = fmul <2 x float> %value, %value
  %negated = fneg <2 x float> %squared
  %updated = insertelement <2 x float> %negated, float %replacement, i64 0
  %lane = extractelement <2 x float> %updated, i64 1
  ret float %lane
}

define <2 x float> @build_vector(float %real, float %imaginary) {
  %first = insertelement <2 x float> poison, float %real, i64 0
  %result = insertelement <2 x float> %first, float %imaginary, i64 1
  ret <2 x float> %result
}

define i32 @vector_gep(ptr %base, i64 %row, i64 %lane) {
  %slot = getelementptr <4 x i32>, ptr %base, i64 %row, i64 %lane
  %value = load i32, ptr %slot
  ret i32 %value
}
