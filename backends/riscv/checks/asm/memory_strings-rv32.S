# RUN: tir mc --march=rv32i --stage=regalloc %s | filecheck %s

    .section .rodata
msg: .string "Hello, RISC-V!"

    .section .bss
    .align 4
buffer: .space 16

    .section .text
    .global _memtest
_memtest:
    la   x1, msg
    la   x2, buffer
    lw   x3, 0(x1)
    sw   x3, 0(x2)
    nop

# CHECK:      asm.symbol {name = "_memtest"}
# CHECK-NEXT: riscv.lw {rd = x3, imm = 0, rs1 = x1}
# CHECK-NEXT: riscv.sw {rs2 = x3, imm = 0, rs1 = x2}
# CHECK:      asm.literal {kind = "string", value = "Hello, RISC-V!"}
# CHECK-NEXT: asm.literal {kind = "space", value = 16}
