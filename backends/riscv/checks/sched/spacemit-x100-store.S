# RUN: tir sched --march=rv64imfdc --mcpu=spacemit-x100 --iterations=200 %s | filecheck %s

.global f
f:
    sb x10, 0(x11)
    sw x10, 4(x11)
    sd x10, 8(x11)
    fsd f10, 16(x11)
    c.sw x10, 24(x11)
    c.sd x10, 32(x11)
    c.fsd f10, 40(x11)
    c.fsdsp f10, 48(sp)

# CHECK: Instructions:      1600
# CHECK: Total Cycles:      1601
