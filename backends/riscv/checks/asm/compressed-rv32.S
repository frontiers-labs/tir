# RUN: tir mc --march=rv32ifc %s | filecheck %s
# RUN: not tir mc --march=rv64gc %s

# The RV32-only compressed forms: c.jal (rv64 reuses its encoding for
# c.addiw) and the Zcf single-precision float loads/stores (rv64 reuses their
# encodings for c.ld/c.sd).

.global f
f:
    c.jal 0
    c.flw fa0, 4(s0)
    c.fsw fa0, 8(s0)
    c.flwsp fa1, 12(sp)
    c.fswsp fa1, 16(sp)

# CHECK: c.jal 0
# CHECK-NEXT: c.flw f10, 4(x8)
# CHECK-NEXT: c.fsw f10, 8(x8)
# CHECK-NEXT: c.flwsp f11, 12(sp)
# CHECK-NEXT: c.fswsp f11, 16(sp)
