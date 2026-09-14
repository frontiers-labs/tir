; RUN: tir mc --march=x86_64 --filetype=obj %s | tir readobj - | filecheck %s

; CHECK: Symbol table: value=0x0 size=0x8 bind=GLOBAL type=OBJECT section=.data
; CHECK: Reloc .data+0x0: reloc(1) external + 0

@external = external global i32
@table = global [1 x ptr] [ptr @external], align 8
