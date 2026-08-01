# RUN: tir sched --march=x86_64 --model=tiger-lake --iterations=100 %s | filecheck %s

.global f
f:
    cvtsi2sd xmm0, rax
    cvttsd2si rbx, xmm1

# CHECK:      [1]    [2]    Instructions:
# CHECK-NEXT: 4      1      cvtsi2sd xmm0, rax
# CHECK-NEXT: 1      1      cvttsd2si rbx, xmm1
# CHECK: [11]: FPConvert
