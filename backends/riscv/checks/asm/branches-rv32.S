# RUN: tir mc --march=rv32i --stage=regalloc %s | filecheck %s

    .section .text
    .global _branch_test
_branch_test:
    addi x1, x0, 3
    addi x2, x0, 3
    bne  x1, x2, mismatch
    addi x3, x0, 1     # equal case: fall through
    jal  x0, done
mismatch:
    addi x3, x0, -1
done:
    addi x0, x0, 0     # nop

# The forward branches to `mismatch` and `done` resolve to local blocks, so the
# immediates print as block references rather than symbol strings.
# CHECK:      riscv.bne {rs1 = x1, rs2 = x2, imm = ^bb1}
# CHECK:      riscv.jal {rd = x0, imm = ^bb2}
# CHECK:      ^bb1 {name = "mismatch"}:
# CHECK-NEXT: riscv.addi {rd = x3, rs1 = x0, imm = -1}
# CHECK:      ^bb2 {name = "done"}:
# CHECK-NEXT: riscv.addi {rd = x0, rs1 = x0, imm = 0}
