# RUN: not isasim --march=riscv64 --entry=_start --until-pc=done --max-cycles=1 %s 2>&1 | filecheck %s

# Exhausting --max-cycles before reaching --until-pc is a trap, not a hang.
.global done
done:
  add x0, x0, x0
.global _start
_start:
  addi x1, x0, 5
  addi x2, x0, 7
  add  x3, x1, x2

# CHECK: MaxCyclesExceeded
