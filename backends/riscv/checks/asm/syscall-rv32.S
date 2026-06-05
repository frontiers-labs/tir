# RUN: tir mc --march=rv32i --stage=regalloc %s | filecheck %s

    .section .text
    .global _exit
_exit:
    addi x10, x0, 0   # return code
    addi x17, x0, 93  # ECALL: exit
    ecall

# CHECK: riscv.addi {rd = x10, rs1 = x0, imm = 0}
# CHECK-NEXT: riscv.addi {rd = x17, rs1 = x0, imm = 93}
# CHECK-NEXT: riscv.ecall
