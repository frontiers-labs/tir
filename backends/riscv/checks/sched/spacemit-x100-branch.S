# RUN: tir sched --march=rv64ic --mcpu=spacemit-x100 --iterations=1 %s | filecheck %s

.global f
f:
    beq x10, x11, 8
    jal x1, 8
    jalr x1, 0(x10)
    c.beqz x10, 8
    c.j 8
    c.jr x10
    c.jalr x10

# CHECK: [1]    [2]    Instructions:
# CHECK-NEXT: 2      1      beq x10, x11, 8
# CHECK-NEXT: 2      1      jal x1, 8
# CHECK-NEXT: 2      1      jalr x1, 0(x10)
# CHECK-NEXT: 2      1      c.beqz x10, 8
# CHECK-NEXT: 2      1      c.j 8
# CHECK-NEXT: 2      1      c.jr x10
# CHECK-NEXT: 2      1      c.jalr x10
