# RUN: tir mc --march=rv32if --stage=regalloc %s | filecheck %s
# RUN: tir mc --march=rv64id --stage=regalloc %s | filecheck %s
# RUN: tir mc --march=rv64i --mattr=+f --stage=regalloc %s | filecheck %s

.global f
f:
    csrrs x10, fcsr, x0

# CHECK: riscv.csrrs {rd = x10, csr = fcsr, rs1 = x0}
