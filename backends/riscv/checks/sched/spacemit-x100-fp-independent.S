# RUN: tir sched --march=rv64imfd --mcpu=spacemit-x100 --iterations=100 %s | filecheck %s

fadd.s f10, f20, f21
fadd.s f11, f20, f21
fadd.s f12, f20, f21
fadd.s f13, f20, f21

# CHECK: Total Cycles:      203
