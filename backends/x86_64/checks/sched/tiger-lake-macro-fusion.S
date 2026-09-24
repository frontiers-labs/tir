# RUN: tir sched --march=x86_64 --model=tiger-lake --iterations=100 %s | filecheck %s

# cmp + jne macro-fuse for timing, while reports retain both architectural
# instructions. test + js fuses too (test pairs with the sign conditions; cmp
# does not, as described by the Tiger Lake fusion groups).
# The fused pair removes the false loop-carried flags dependency, reducing this
# repeated stream from the prior 101-cycle estimate to 58 cycles.

.global f
f:
    cmp rax, rbx
    jne 16

# CHECK: Instructions:      200
# CHECK: Decoded uops:      100
# CHECK: Execution uops:    100
# CHECK: Fused groups:      100
# CHECK: Total Cycles:      58
# CHECK: [1]    [2]    Instructions:
# CHECK-NEXT: 1      1      cmp rax, rbx
# CHECK-NEXT: 1      1      jne 16
# CHECK: Resource pressure by instruction:
# CHECK-NEXT: [0]   [1]   [2]   [3]   Instructions:
# CHECK-NEXT: -     -     -     -     cmp rax, rbx
# CHECK-NEXT: -     -     0.50  0.50  jne 16
