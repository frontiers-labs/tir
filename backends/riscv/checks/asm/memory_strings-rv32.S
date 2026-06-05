# RUN: tir mc --march=rv32i --stage=regalloc %s | filecheck %s

    .section .rodata
msg: .string "Hello, RISC-V!"

    .section .bss
    .align 4
buffer: .space 16

    .section .text
    .global _memtest
_memtest:
    addi x1, x0, 0
    addi x2, x0, 16
    lw   x3, 0(x1)
    sw   x3, 0(x2)
    addi x0, x0, 0

# CHECK:      riscv.addi {rd = x1, rs1 = x0, imm = 0}
# CHECK-NEXT: riscv.addi {rd = x2, rs1 = x0, imm = 16}
# CHECK-NEXT: riscv.lw {rd = x3, imm = 0, rs1 = x1}
# CHECK-NEXT: riscv.sw {rs2 = x3, imm = 0, rs1 = x2}
# CHECK-NEXT: riscv.addi {rd = x0, rs1 = x0, imm = 0}
