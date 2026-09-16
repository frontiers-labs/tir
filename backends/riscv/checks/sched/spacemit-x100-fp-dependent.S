# RUN: tir sched --march=rv64imfd --mcpu=spacemit-x100 --iterations=100 %s | filecheck %s

fadd.s f10, f10, f21
fadd.s f10, f10, f21
fadd.s f10, f10, f21
fadd.s f10, f10, f21

# CHECK: Total Cycles:      1201
