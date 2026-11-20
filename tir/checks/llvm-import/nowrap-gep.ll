; IR migration: instruction selection and backend lowering are unavailable.
; XFAIL: *
; RUN: tir llvm-import %s | tir opt --verify | filecheck %s --check-prefix=IR
; RUN: tir mc --march=x86_64 --filetype=asm %s | filecheck %s --check-prefix=ASM

define i32 @scaled_nsw(ptr %base, i32 %x) {
  %n = add nsw i32 %x, 5
  %wide = sext i32 %n to i64
  %original = sext i32 %x to i64
  %a = getelementptr [50 x i32], ptr %base, i64 %wide
  %b = getelementptr [50 x i32], ptr %base, i64 %original
  %va = load i32, ptr %a
  %vb = load i32, ptr %b
  %sum = add i32 %va, %vb
  ret i32 %sum
}

define ptr @commuted_cross_block(ptr %base, i32 %x) {
entry:
  %n = add nsw i32 5, %x
  br label %next
next:
  %wide = sext i32 %n to i64
  %p = getelementptr i32, ptr %base, i64 %wide
  ret ptr %p
}

define ptr @negative_offset(ptr %base, i8 %x) {
  %n = add nsw i8 %x, 255
  %wide = sext i8 %n to i64
  %p = getelementptr i32, ptr %base, i64 %wide
  ret ptr %p
}

define ptr @wrapped_offset(ptr %base, i8 %x) {
  %n = add i8 %x, 1
  %wide = sext i8 %n to i64
  %p = getelementptr i32, ptr %base, i64 %wide
  ret ptr %p
}

define ptr @unsigned_offset(ptr %base, i8 %x) {
  %n = add nuw i8 %x, 1
  %wide = sext i8 %n to i64
  %p = getelementptr i32, ptr %base, i64 %wide
  ret ptr %p
}

define i32 @mixed_field(ptr %base, i64 %index) {
  %p = getelementptr [4 x {i32, i32}], ptr %base, i64 0, i64 %index, i32 1
  %value = load i32, ptr %p
  ret i32 %value
}

; IR-LABEL: func.func @scaled_nsw
; IR: constant {value = 1000} : !i64
; IR-LABEL: func.func @commuted_cross_block
; IR: constant {value = 20} : !i64
; IR-LABEL: func.func @negative_offset
; IR: constant {value = -4} : !i64
; IR-LABEL: func.func @wrapped_offset
; IR: %[[N:[0-9]+]] = addi %{{[0-9]+}}, %{{[0-9]+}} : !i8
; IR: extsi %[[N]] : !i64
; IR-LABEL: func.func @unsigned_offset
; IR: %[[N:[0-9]+]] = addi %{{[0-9]+}}, %{{[0-9]+}} : !i8
; IR: extsi %[[N]] : !i64

; The two row addresses share x*200 instead of computing both x*200 and (x+5)*200.
; ASM-LABEL: scaled_nsw:
; ASM: imul
; ASM-NOT: imul
; ASM: + 1000]
; ASM-NOT: imul
; ASM: ret
; ASM-LABEL: mixed_field:
; ASM-NEXT: mov {{[a-z0-9]+}}, [rdi + 8*rsi + 4]
; ASM-NEXT: ret
