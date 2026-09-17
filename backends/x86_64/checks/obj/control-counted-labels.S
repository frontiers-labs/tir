# RUN: tir mc --march=x86_64 --filetype=obj -o - %s | tir readobj - | filecheck %s

.global counted_transfer
counted_transfer:
    loop external

# CHECK: Reloc .text+0x1: reloc(15) external + -1
