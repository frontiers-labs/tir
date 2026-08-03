# RUN: tir sched --march=x86_64 --model=tiger-lake --iterations=100 %s | filecheck %s

# cmp + jne macro-fuse: the pair decodes, issues, and retires as one micro-op
# on the branch units, so 100 iterations of the pair retire one per cycle
# (Instructions counts retired micro-ops). test + js fuses too (test pairs
# with the sign conditions; cmp does not — see the TigerLake fusion groups).

.global f
f:
    cmp rax, rbx
    jne 16

# CHECK: Instructions:      100
# CHECK: Total Cycles:      101
# CHECK: [1]    [2]    Instructions:
# CHECK-NEXT: 1      1      cmp rax, rbx; jne 16
