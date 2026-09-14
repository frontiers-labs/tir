; RUN: tir llvm-import %s | tir opt --verify | filecheck %s

; CHECK: func.func @choose
; CHECK: constant {value = 7}
; CHECK: cfg.cond_br

define i32 @choose(i32 %x) {
entry.loop:
  switch i32 %x, label %done [
    i32 0, label %done
    i32 1, label %done
  ]
done:
  %result = phi i32 [ 7, %entry.loop ]
  ret i32 %result
}
