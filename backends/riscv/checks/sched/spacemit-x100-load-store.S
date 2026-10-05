# RUN: tir sched --march=rv64imfdc --mcpu=spacemit-x100 --iterations=200 %s | filecheck %s

.global f
f:
    ld x10, 0(x11)
    ld x12, 8(x11)
    sd x13, 16(x11)

# CHECK: Instructions:      600
# CHECK: Total Cycles:      303
