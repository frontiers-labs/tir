; RUN: tir mc --march=x86_64 --filetype=asm %s llvm | filecheck %s

; CHECK-LABEL: load_extend:
; CHECK-NEXT: movzx eax, word ptr [rdi + 2*rsi]
; CHECK-NEXT: ret
; CHECK-LABEL: load_signed:
; CHECK-NEXT: movsx eax, word ptr [rdi + 2*rsi]
; CHECK-NEXT: ret
; CHECK-LABEL: load_before_store:
; CHECK-NEXT: movzx eax, word ptr [rdi + 2*rsi]
; CHECK-NEXT: mov [rdi + 2*rsi], dx
; CHECK-NEXT: ret
; CHECK-LABEL: shared_load:
; CHECK: movzx eax, word ptr [rdi]
; CHECK-NEXT: mov [rsi], ax
; CHECK: ret

define i32 @load_extend(ptr %base, i64 %idx) {
 %p=getelementptr i16, ptr %base, i64 %idx
 %v=load i16, ptr %p
 %r=zext i16 %v to i32
 ret i32 %r
}
define i32 @load_signed(ptr %base, i64 %idx) {
 %p=getelementptr i16, ptr %base, i64 %idx
 %v=load i16, ptr %p
 %r=sext i16 %v to i32
 ret i32 %r
}
define i32 @load_before_store(ptr %base, i64 %idx, i16 %new) {
 %p=getelementptr i16, ptr %base, i64 %idx
 %v=load i16, ptr %p
 store i16 %new, ptr %p
 %r=zext i16 %v to i32
 ret i32 %r
}
define i32 @shared_load(ptr %base, ptr %out) {
 %v=load i16, ptr %base
 store i16 %v, ptr %out
 %r=zext i16 %v to i32
 ret i32 %r
}
