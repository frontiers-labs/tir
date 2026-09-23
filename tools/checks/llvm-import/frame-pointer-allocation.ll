; RUN: tir mc --march=x86_64 --filetype=asm %s llvm | filecheck %s

; Two computed values survive a call. Both ordinary callee-saved registers,
; including the unused frame pointer, must retain their caller's values.
; CHECK-LABEL: saved_frame_pointer:
; CHECK: push rbx
; CHECK-NEXT: push rbp
; CHECK: mov rbp, rdi
; CHECK-NEXT: sub rbp, rsi
; CHECK: call external
; CHECK: imul {{.*}}, rbp
; CHECK: pop rbp
; CHECK-NEXT: pop rbx
; CHECK-NEXT: ret

declare i64 @external(i64)
define i64 @saved_frame_pointer(i64 %a, i64 %b) {
 %sum = add i64 %a, %b
 %delta = sub i64 %a, %b
 %call = call i64 @external(i64 %sum)
 %product = mul i64 %sum, %delta
 %result = add i64 %product, %call
 ret i64 %result
}
