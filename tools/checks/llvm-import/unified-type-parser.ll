; RUN: tir llvm-import %s | tir opt --verify | filecheck %s

%pair = type { i32, i32 }

@pair = global %pair zeroinitializer, align 4
@pointer = global i8* null, align 8

declare i8* @identity(i8*)

define void @allocate() {
  %value = alloca { i32, i32 }, align 4
  ret void
}

; CHECK: global @pair size 8 align 4
; CHECK: global @pointer size 8 align 8
; CHECK: func @allocate
; CHECK: ptr.alloca {size = 8, align = 4}
