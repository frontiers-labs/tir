# RUN: tir sched --march=rv64imfd --mcpu=spacemit-x100 --iterations=10 %s | filecheck %s

.global f
f:
    fadd.s f10, f11, f12
    fsub.d f10, f11, f12, rne
    fmul.s f10, f11, f12
    fmadd.s f10, f11, f12, f13
    fmadd.d f10, f11, f12, f13
    fcvt.d.w f10, x10
    fsgnj.d f10, f11, f12
    feq.d x10, f11, f12
    fmv.d.x f10, x11

# CHECK: [1]    [2]    Instructions:
# CHECK-NEXT: 3      1      fadd.s f10, f11, f12
# CHECK-NEXT: 3      1      fsub.d f10, f11, f12, rne
# CHECK-NEXT: 4      1      fmul.s f10, f11, f12
# CHECK-NEXT: 5      1      fmadd.s f10, f11, f12, f13
# CHECK-NEXT: 4      1      fmadd.d f10, f11, f12, f13
# CHECK-NEXT: 3      1      fcvt.d.w f10, x10
# CHECK-NEXT: 3      1      fsgnj.d f10, f11, f12
# CHECK-NEXT: 3      1      feq.d x10, f11, f12
# CHECK-NEXT: 3      1      fmv.d.x f10, x11
