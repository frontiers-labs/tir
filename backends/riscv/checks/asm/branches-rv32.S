# RUN: tir mc --march=rv32i --stage=regalloc %s | filecheck %s

    .section .text
    .global _branch_test
_branch_test:
    addi x1, x0, 3
    addi x2, x0, 3
    bne  x1, x2, 8
    addi x3, x0, 1
    jal  x0, 8
mismatch:
    addi x3, x0, -1
done:
    addi x0, x0, 0

# CHECK:      riscv.addi {rd = x1, rs1 = x0, imm = 3}
# CHECK-NEXT: riscv.addi {rd = x2, rs1 = x0, imm = 3}
# CHECK-NEXT: riscv.bne {rs1 = x1, rs2 = x2, imm = 8}
# CHECK-NEXT: riscv.addi {rd = x3, rs1 = x0, imm = 1}
# CHECK-NEXT: riscv.jal {rd = x0, imm = 8}
# CHECK-NEXT: riscv.addi {rd = x3, rs1 = x0, imm = -1}
# CHECK-NEXT: riscv.addi {rd = x0, rs1 = x0, imm = 0}
