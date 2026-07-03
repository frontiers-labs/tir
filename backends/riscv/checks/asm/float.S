# RUN: tir mc --march=rv64ifd %s | filecheck %s
# RUN: tir mc --march=rv32ifd %s | filecheck %s
# RUN: tir mc --march=rv64i --mattr=+d %s | filecheck %s
# RUN: not tir mc --march=rv64if %s
# RUN: not tir mc --march=rv64i %s

.global f
f:
    fadd.s fa0, fa1, fa2
    fsub.s ft0, fa0, fa1
    fmv.w.x fa4, a0
    fmul.d ft1, ft2, ft3
    fdiv.d fs0, fs1, fa0
    flw fa3, 8(sp)
    fsw fa3, 12(sp)
    fld ft4, 16(sp)
    fsd ft4, 24(sp)

# CHECK: fadd.s f10, f11, f12
# CHECK: fsub.s f0, f10, f11
# CHECK: fmv.w.x f14, x10
# CHECK: fmul.d f1, f2, f3
# CHECK: fdiv.d f8, f9, f10
# CHECK: flw f13, 8(x2)
# CHECK: fsw f13, 12(x2)
# CHECK: fld f4, 16(x2)
# CHECK: fsd f4, 24(x2)
