# RUN: isasim --march=riscv64 --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s

# Counter CSRs report instructions retired before the reading instruction;
# mscratch behaves as an ordinary read-write CSR.
.global done
done:
  add x0, x0, x0
.global _start
_start:
  addi  x1, x0, 5
  csrrw x0, mscratch, x1
  csrrs x2, mscratch, x0
  csrrs x3, instret, x0
  csrrs x4, cycle, x0

# CHECK: final registers:
# CHECK-DAG: CSR[832] = 0x5
# CHECK-DAG: GPR[2] = 0x5
# CHECK-DAG: GPR[3] = 0x3
# CHECK-DAG: GPR[4] = 0x4
