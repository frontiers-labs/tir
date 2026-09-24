# RUN: tir sched --march=x86_64 --model=tiger-lake --iterations=100 %s | filecheck %s

# A compare and branch in separate symbols cannot fuse, even though their
# opcodes and operands would otherwise match the Tiger Lake rule.
.global compare_only
compare_only:
  cmp rax, rbx
.global branch_only
branch_only:
  jne 16

# CHECK: Instructions:      200
# CHECK: [1]    [2]    Instructions:
# CHECK-NEXT: 1      1      jne 16
# CHECK-NEXT: 1      1      cmp rax, rbx
