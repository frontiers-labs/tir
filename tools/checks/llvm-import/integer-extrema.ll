; RUN: tir mc --march x86_64 --filetype obj %s llvm -o /tmp/tir-integer-extrema-lit.o
; RUN: cc /tmp/tir-integer-extrema-lit.o %S/Inputs/integer-extrema-harness.c -o /tmp/tir-integer-extrema-lit
; RUN: /tmp/tir-integer-extrema-lit

declare i32 @llvm.smin.i32(i32, i32)
declare i32 @llvm.smax.i32(i32, i32)
declare i32 @llvm.umin.i32(i32, i32)

define i32 @signed_min(i32 %left, i32 %right) {
  %result = call i32 @llvm.smin.i32(i32 %left, i32 %right)
  ret i32 %result
}

define i32 @signed_max(i32 %left, i32 %right) {
  %result = call i32 @llvm.smax.i32(i32 %left, i32 %right)
  ret i32 %result
}

define i32 @unsigned_min(i32 %left, i32 %right) {
  %result = call i32 @llvm.umin.i32(i32 %left, i32 %right)
  ret i32 %result
}
