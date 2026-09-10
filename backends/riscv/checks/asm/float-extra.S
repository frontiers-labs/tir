# RUN: tir mc --march=rv64ifd %s | filecheck %s
# RUN: tir mc --march=rv32ifd %s | filecheck %s

.global f
f:
    flt.s a0, fa0, fa1
    fle.s a0, fa0, fa1
    feq.s a0, fa0, fa1
    fmin.s fa0, fa1, fa2
    fmax.s fa0, fa1, fa2
    fmin.d fa0, fa1, fa2
    fmax.d fa0, fa1, fa2
    fsgnj.s fa0, fa1, fa2
    fsgnjn.s fa0, fa1, fa2
    fsgnjx.s fa0, fa1, fa2
    fsgnj.d fa0, fa1, fa2
    fsgnjn.d fa0, fa1, fa2
    fsgnjx.d fa0, fa1, fa2
    fclass.s a0, fa0
    fclass.d a0, fa0
    fmv.x.w a0, fa0

# CHECK: flt.s x10, f10, f11
# CHECK: fle.s x10, f10, f11
# CHECK: feq.s x10, f10, f11
# CHECK: fmin.s f10, f11, f12
# CHECK: fmax.s f10, f11, f12
# CHECK: fmin.d f10, f11, f12
# CHECK: fmax.d f10, f11, f12
# CHECK: fsgnj.s f10, f11, f12
# CHECK: fsgnjn.s f10, f11, f12
# CHECK: fsgnjx.s f10, f11, f12
# CHECK: fsgnj.d f10, f11, f12
# CHECK: fsgnjn.d f10, f11, f12
# CHECK: fsgnjx.d f10, f11, f12
# CHECK: fclass.s x10, f10
# CHECK: fclass.d x10, f10
# CHECK: fmv.x.w x10, f10
