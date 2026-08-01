# RUN: tir sched --march=x86_64 --model=tiger-lake --iterations=100 %s | filecheck %s

.global f
f:
    addsd xmm0, xmm1
    subsd xmm2, xmm3
    mulsd xmm4, xmm5
    divsd xmm6, xmm7

# CHECK:      [1]    [2]    Instructions:
# CHECK-NEXT: 4      1      addsd xmm0, xmm1
# CHECK-NEXT: 4      1      subsd xmm2, xmm3
# CHECK-NEXT: 4      1      mulsd xmm4, xmm5
# CHECK-NEXT: 13     1      divsd xmm6, xmm7
