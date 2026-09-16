# RUN: isasim --march=rv64im --entry=independent --until-pc=independent_done --timing --machine=spacemit-x100 %s | filecheck %s --check-prefix=INDEPENDENT
# RUN: isasim --march=rv64im --entry=dependent --until-pc=dependent_done --timing --machine=spacemit-x100 %s | filecheck %s --check-prefix=DEPENDENT

.global independent_done
independent_done:
  add x0, x0, x0
.global independent
independent:
  addi  x1, x0, 1
  slli  x1, x1, 32
  divw  x10, x20, x1
  divuw x11, x20, x1
  remw  x12, x20, x1
  remuw x13, x20, x1
  divw  x14, x20, x1
  divuw x15, x20, x1
  remw  x16, x20, x1
  remuw x17, x20, x1

.global dependent_done
dependent_done:
  add x0, x0, x0
.global dependent
dependent:
  addi  x1, x0, 1
  slli  x1, x1, 32
  divw  x10, x10, x1
  divuw x10, x10, x1
  remw  x10, x10, x1
  remuw x10, x10, x1
  divw  x10, x10, x1
  divuw x10, x10, x1
  remw  x10, x10, x1
  remuw x10, x10, x1

# INDEPENDENT: 10 instructions, 36 cycles
# DEPENDENT: 10 instructions, 43 cycles
