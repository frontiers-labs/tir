; RUN: not tir llvm-import %s

; `freeze` has no TIR equivalent today, so the import must fail rather than drop
; the instruction.
define i32 @d(i32 %a, i32 %b) {
  %r = freeze i32 %a
  ret i32 %r
}
