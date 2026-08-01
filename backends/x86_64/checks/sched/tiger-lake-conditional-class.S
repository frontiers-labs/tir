# RUN: tir sched --march=x86_64 --model=tiger-lake --iterations=100 %s | filecheck %s

.global f
f:
    sete al
    cmove rax, rbx

# CHECK: [10]: IConditional
# CHECK: Resource pressure by instruction:
# CHECK: 1.00{{.*}}sete al
# CHECK: 1.00{{.*}}cmove rax, rbx
