; RUN: tir llvm-import %s | tir opt --verify | filecheck %s

@message = constant [9 x i8] c"range(1)\00"
@gep_message = constant [16 x i8] c"getelementptr()\00"

; CHECK: global @message align 1 section ".rodata" bytes [114, 97, 110, 103, 101, 40, 49, 41, 0]
; CHECK: global @gep_message align 1 section ".rodata" bytes [103, 101, 116, 101, 108, 101, 109, 101, 110, 116, 112, 116, 114, 40, 41, 0]
