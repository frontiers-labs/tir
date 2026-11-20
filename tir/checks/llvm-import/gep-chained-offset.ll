; IR migration: instruction selection and backend lowering are unavailable.
; XFAIL: *
; RUN: tir llvm-import %s | tir opt --verify | filecheck %s --check-prefix=IR
; RUN: tir mc --march=x86_64 --filetype=asm %s | filecheck %s
; RUN: tir mc --march=x86_64 --shuffle-machine-order --filetype=obj %s -o /dev/null

; CHECK-LABEL: chained:
; CHECK-NOT: lea
; CHECK: mov {{.*}}, [rdi + 4*rsi + 5000]
; CHECK-NEXT: ret

define i32 @chained(ptr %base, i64 %index) {
  %offset = getelementptr i8, ptr %base, i64 5000
  %address = getelementptr i32, ptr %offset, i64 %index
  %value = load i32, ptr %address
  ret i32 %value
}

; The intermediate address also has a use and must keep its original offset.
define i32 @shared_offset(ptr %base, i64 %index) {
  %offset = getelementptr i8, ptr %base, i64 12
  %address = getelementptr i32, ptr %offset, i64 %index
  %first = load i32, ptr %offset
  %second = load i32, ptr %address
  %sum = add i32 %first, %second
  ret i32 %sum
}

define i32 @negative_offset(ptr %base, i64 %index) {
  %offset = getelementptr i8, ptr %base, i8 252
  %address = getelementptr i32, ptr %offset, i64 %index
  %value = load i32, ptr %address
  ret i32 %value
}

; Constant byte offsets add modulo 2^64, including signed overflow.
define ptr @wrapped_offsets(ptr %base) {
  %a = getelementptr i8, ptr %base, i64 9223372036854775807
  %b = getelementptr i8, ptr %a, i64 1
  %c = getelementptr i8, ptr %b, i64 -9223372036854775808
  ret ptr %c
}

; IR: func.func @chained
; CHECK-LABEL: wrapped_offsets:
; CHECK-NOT: add
; CHECK-NOT: lea
; CHECK: ret
