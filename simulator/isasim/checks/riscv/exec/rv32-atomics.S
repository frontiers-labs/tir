# RUN: isasim --march=rv32ia --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s

# The A extension on rv32: only the word (.w) forms exist. LR/SC increments the
# word at a0 (0x03020100 -> 0x03020101, SC success -> t1 = 0), then two AMOs
# return the old memory value. Registers are 32 bits wide here.
.global done
done:
  add x0, x0, x0
.global tail
tail:
  amoadd.w  a2, t3, (a1)     # old 0x03020100 -> a2; mem += 10
  amoswap.w a3, t3, (a1)     # old 0x0302010a -> a3; mem = 10
.global loop
loop:
  lr.w      t0, (a0)
  addi      t0, t0, 1
  sc.w      t1, t0, (a0)
  bne       t1, x0, -12
.global _start
_start:
  addi      t3, x0, 10

# CHECK: final registers:
# CHECK-DAG: GPR[5] = 0x3020101 (width=32)
# CHECK-DAG: GPR[6] = 0x0 (width=32)
# CHECK-DAG: GPR[12] = 0x3020100 (width=32)
# CHECK-DAG: GPR[13] = 0x302010a (width=32)
