; RUN: tir llvm-import %s | tir opt --verify | filecheck %s

; CHECK: func.func @choose
; CHECK: alloca
; CHECK: cfg.cond_br
; CHECK: store
; CHECK: cfg.br ^
; CHECK: load
; CHECK: func.return

define i32 @choose(i1 %c, i32 %x, i32 %y) {
entry:
  %p = alloca i32
  br i1 %c, label %t, label %f
t:
  store i32 %x, ptr %p
  br label %done
f:
  store i32 %y, ptr %p
  br label %done
done:
  %r = load i32, ptr %p
  ret i32 %r
}
