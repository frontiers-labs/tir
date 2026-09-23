; RUN: env TIR_VERIFY_AXIOMS=1 tir mc --march=x86_64 --filetype=asm %s llvm | filecheck %s

; Only low bytes matter, even when the high bits differ.
define i1 @same_byte(i16 %a, i16 %b) {
  %x = xor i16 %a, %b
  %low = and i16 %x, 255
  %r = icmp eq i16 %low, 0
  ret i1 %r
}
; CHECK-LABEL: same_byte:
; CHECK-NEXT: cmp dil, sil
; CHECK-NEXT: sete al
; CHECK-NEXT: ret

define i1 @different_byte(i32 %a, i32 %b) {
  %x = xor i32 %a, %b
  %low = and i32 %x, 255
  %r = icmp ne i32 %low, 0
  ret i1 %r
}
; CHECK-LABEL: different_byte:
; CHECK-NEXT: cmp dil, sil
; CHECK-NEXT: setne al
; CHECK-NEXT: ret
