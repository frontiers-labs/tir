# RUN: tir sched --march=rv64ic --mcpu=spacemit-x100 --iterations=100 %s | filecheck %s

# These isolated regions contain no eligible adjacent pair. In particular,
# the reversed pair has no left-right match within its own region.
.global different_register
 different_register:
    c.slli a1, 59
    c.srli a2, 62
.global decreasing_amount
 decreasing_amount:
    c.slli a1, 59
    c.srli a1, 54
.global reverse_order
 reverse_order:
    c.srli a1, 62
    c.slli a1, 59
.global full_width
 full_width:
    slli a1, a1, 59
    srli a1, a1, 62
.global zero_left
 zero_left:
    c.slli a1, 0
    c.srli a1, 1
.global zero_right
 zero_right:
    c.slli a1, 0
    c.srli a1, 0

# CHECK: Instructions:      1200
# CHECK: Decoded uops:      1200
# CHECK: Execution uops:    1200
# CHECK: Fused groups:      0
