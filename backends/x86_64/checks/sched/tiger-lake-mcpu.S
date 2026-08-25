# RUN: tir sched --march=x86_64 --model=tiger-lake --iterations=100 %s | filecheck %s
# RUN: tir sched --march=x86_64 --mcpu=tiger-lake --iterations=100 %s | filecheck %s

# --mcpu implies the machine to analyze against, so naming the model explicitly
# and naming the cpu resolve to the same machine and report the same schedule.

.global f
f:
    add rax, rbx

# CHECK: Total Cycles:      101
# CHECK: Dispatch Width:    5
