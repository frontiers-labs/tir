; RUN: tir llvm-import %s | filecheck %s

define void @aligned_buffer() {
entry:
  %buffer = alloca [31 x i8], align 16
  ret void
}

; CHECK: ptr.alloca {size = 31, align = 16}
