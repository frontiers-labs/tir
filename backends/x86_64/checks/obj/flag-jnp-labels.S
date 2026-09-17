# RUN: tir mc --march=x86_64 --stage=isel %s | filecheck %s --check-prefix=IR
# RUN: tir mc --march=x86_64 --filetype=obj-ascii %s | filecheck %s --check-prefix=ENC
# RUN: tir mc --march=x86_64 --filetype=obj -o - %s | tir readobj - | filecheck %s --check-prefix=RELOC

.global parity_jump
parity_jump:
back:
    jnp forward
    jnp back
forward:
    jnp external

# IR: x86_64.jnp
# ENC: [0x0F, 0x8B, 0x06, 0x00, 0x00, 0x00]
# ENC-NEXT: [0x0F, 0x8B, 0xF4, 0xFF, 0xFF, 0xFF]
# ENC-NEXT: [0x0F, 0x8B, 0x00, 0x00, 0x00, 0x00]
# RELOC: Reloc .text+0xe: R_X86_64_PC32 external + -4
