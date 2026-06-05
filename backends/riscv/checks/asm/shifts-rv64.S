# RUN: tir mc --march=rv64i --stage=regalloc %s | filecheck %s

    .section .text
shift_test:
    addi x1, x0, 64
    slli x2, x1, 8
    srli x3, x2, 2
    slt  x4, x1, x2
    sltu x5, x1, x3

# CHECK:      riscv.addi {rd = x1, rs1 = x0, imm = 64}
# CHECK-NEXT: riscv.slli {rd = x2, rs1 = x1, imm = 8}
# CHECK-NEXT: riscv.srli {rd = x3, rs1 = x2, imm = 2}
# CHECK-NEXT: riscv.slt {rd = x4, rs1 = x1, rs2 = x2}
# CHECK-NEXT: riscv.sltu {rd = x5, rs1 = x1, rs2 = x3}
