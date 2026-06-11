# RUN: tir mc --march=rv32i_zicsr --stage=regalloc %s | filecheck %s
# RUN: not tir mc --march=rv32i %s

.global f
f:
    csrrw  x5, mscratch, x6
    csrrs  x7, cycle, x0
    csrrc  x8, instret, x9
    csrrwi x10, time, 1
    csrrsi x11, mscratch, 2
    csrrci x12, mscratch, 31
    csrrs  x13, cycleh, x0
    csrrs  x14, instreth, x0

# CHECK:      riscv.csrrw {rd = x5, csr = mscratch, rs1 = x6}
# CHECK-NEXT: riscv.csrrs {rd = x7, csr = cycle, rs1 = x0}
# CHECK-NEXT: riscv.csrrc {rd = x8, csr = instret, rs1 = x9}
# CHECK-NEXT: riscv.csrrwi {rd = x10, csr = time, zimm = 1}
# CHECK-NEXT: riscv.csrrsi {rd = x11, csr = mscratch, zimm = 2}
# CHECK-NEXT: riscv.csrrci {rd = x12, csr = mscratch, zimm = 31}
# CHECK-NEXT: riscv.csrrs {rd = x13, csr = cycleh, rs1 = x0}
# CHECK-NEXT: riscv.csrrs {rd = x14, csr = instreth, rs1 = x0}
