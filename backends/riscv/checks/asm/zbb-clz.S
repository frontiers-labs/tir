# RUN: tir mc --march=rv32i_zbb %s | filecheck %s
# RUN: tir mc --march=rv64i_zbb %s | filecheck %s
# RUN: tir mc --march=rv32i --mattr=+zbb %s | filecheck %s
# RUN: tir mc --march=rv64i --mattr=+zbb %s | filecheck %s
# RUN: not tir mc --march=rv32i_zbb --mattr=-zbb %s
# RUN: not tir mc --march=rv64i_zbb --mattr=-zbb %s

.global f
f:
    clz x10, x11
    clz x0, x31
    clz x31, x0

# CHECK: clz x10, x11
# CHECK-NEXT: clz x0, x31
# CHECK-NEXT: clz x31, x0
