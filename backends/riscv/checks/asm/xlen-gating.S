# RV64-only instructions must not be available on rv32 targets.
# RUN: tir mc --march=rv64i %s | filecheck %s
# RUN: not tir mc --march=rv32i %s
# RUN: not tir mc --march=rv32im %s

.global f
f:
    addw x10, x11, x12
    ld x11, 0(x2)

# CHECK: addw x10, x11, x12
# CHECK: ld x11, 0(x2)
