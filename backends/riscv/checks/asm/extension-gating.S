# RUN: tir mc --march=rv64im %s | filecheck %s
# RUN: tir mc --march=rv64i --mattr=+m %s | filecheck %s
# RUN: tir mc --march=rv32i_zmmul %s | filecheck %s
# RUN: not tir mc --march=rv64i %s
# RUN: not tir mc --march=rv64im --mattr=-m,-zmmul %s

.global f
f:
    mul x10, x11, x12

# CHECK: mul x10, x11, x12
