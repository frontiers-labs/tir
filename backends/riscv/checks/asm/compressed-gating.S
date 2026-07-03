# RUN: tir mc --march=rv64ic %s | filecheck %s
# RUN: tir mc --march=rv64i --mattr=+c %s | filecheck %s
# RUN: not tir mc --march=rv64i %s
# RUN: not tir mc --march=rv64ic --mattr=-c %s

.global f
f:
    c.addi a0, 1

# CHECK: c.addi x10, 1
