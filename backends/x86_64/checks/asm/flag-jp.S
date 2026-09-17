# RUN: tir mc --march=x86_64 --stage=isel %s | filecheck %s --check-prefix=IR
# RUN: tir mc --march=x86_64 --filetype=obj-ascii %s | filecheck %s --check-prefix=ENC

.global parity_jump
parity_jump:
    jp 16

# IR: x86_64.jp {imm = 16}
# ENC: [0x0F, 0x8A, 0x10, 0x00, 0x00, 0x00]
