# RUN: tir sched --march=rv64imfd --mcpu=spacemit-x100 --iterations=1 %s | filecheck %s

.global f
f:
    div x10, x11, x12
    remu x10, x11, x12
    divw x10, x11, x12
    remuw x10, x11, x12
    fdiv.s f10, f11, f12
    fsqrt.d f10, f11

# CHECK: [1]    [2]    Instructions:
# CHECK-NEXT: 22     1      div x10, x11, x12
# CHECK-NEXT: 22     1      remu x10, x11, x12
# CHECK-NEXT: 14     1      divw x10, x11, x12
# CHECK-NEXT: 14     1      remuw x10, x11, x12
# CHECK-NEXT: 15     1      fdiv.s f10, f11, f12
# CHECK-NEXT: 23     1      fsqrt.d f10, f11
