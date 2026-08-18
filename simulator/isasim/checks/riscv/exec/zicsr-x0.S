# RUN: isasim --march=rv64i_zicsr --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s

# csrrs/csrrsi with a zero source read the CSR into rd but must not write it;
# a non-x0 source still sets bits.
.global done
done:
  add x0, x0, x0
.global first
first:
  addi   x1, x0, 10        # 0b1010
  csrrw  x0, mscratch, x1  # mscratch = 0b1010
  addi   x4, x0, 5         # 0b0101
  csrrs  x2, mscratch, x0  # read only
  csrrsi x3, mscratch, 0   # read only
  csrrs  x5, mscratch, x4  # x5 = 0b1010, mscratch = 0b1111
  csrrs  x6, mscratch, x0  # x6 = 0b1111 (read back)
.global _start
_start:
  add x0, x0, x0

# CHECK: final registers:
# CHECK-DAG: GPR[2] = 0xa
# CHECK-DAG: GPR[3] = 0xa
# CHECK-DAG: GPR[5] = 0xa
# CHECK-DAG: GPR[6] = 0xf
