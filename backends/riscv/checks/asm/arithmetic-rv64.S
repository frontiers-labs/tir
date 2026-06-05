# RUN: tir mc --march=rv64i --stage=regalloc %s | filecheck %s

    .section .data
bigval: .dword 0x100000000

    .section .text
    .global _start64
_start64:
    la   x5, bigval
    ld   x6, 0(x5)
    addi x7, x6, 42
    sd   x7, 0(x5)

# CHECK:      riscv.addi {rd = x5, rs1 = x0, imm = 0}
# CHECK-NEXT: riscv.ld {rd = x6, imm = 0, rs1 = x5}
# CHECK-NEXT: riscv.addi {rd = x7, rs1 = x6, imm = 42}
# CHECK-NEXT: riscv.sd {rs2 = x7, imm = 0, rs1 = x5}
