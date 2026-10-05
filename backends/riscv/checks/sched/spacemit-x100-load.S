# RUN: tir sched --march=rv64imfdc --mcpu=spacemit-x100 --iterations=1 %s | filecheck %s
# RUN: tir sched --march=rv64imfdc --mcpu=spacemit-x100 --iterations=200 %s | filecheck --check-prefix=THROUGHPUT %s

.global f
f:
    ld x10, 0(x11)
    fld f10, 0(x11)
    flw f10, 0(x11)
    c.fld f10, 0(x11)
    c.fldsp f10, 0(sp)

# CHECK: [1]    [2]    Instructions:
# CHECK-NEXT: 3      1      ld x10, 0(x11)
# CHECK-NEXT: 4      1      fld f10, 0(x11)
# CHECK-NEXT: 4      1      flw f10, 0(x11)
# CHECK-NEXT: 4      1      c.fld f10, 0(x11)
# CHECK-NEXT: 4      1      c.fldsp f10, 0(sp)

# THROUGHPUT: Instructions:      1000
# THROUGHPUT: Total Cycles:      504
