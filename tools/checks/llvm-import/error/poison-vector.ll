; RUN: not tir llvm-import %s 2>&1 | filecheck %s

define <2 x float> @partially_poison(float %value) {
  %result = insertelement <2 x float> poison, float %value, i64 0
  ret <2 x float> %result
}

; CHECK: unsupported instruction: partially poison vector value
