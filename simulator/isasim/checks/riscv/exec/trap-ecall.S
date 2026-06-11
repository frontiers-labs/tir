# RUN: isasim --march=riscv64 --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s

# ecall traps to the embedder; isasim reports it and halts the run, so the
# instruction after the ecall never executes.
.global done
done:
  add x0, x0, x0
.global _start
_start:
  addi x17, x0, 93
  ecall
  addi x1, x0, 1

# CHECK: trap: cause=11 pc=0x80000004
# CHECK: final registers:
# CHECK-NOT: GPR[1]
# CHECK: GPR[17] = 0x5d
