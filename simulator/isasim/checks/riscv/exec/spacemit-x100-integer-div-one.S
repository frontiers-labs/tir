# RUN: isasim --march=rv64im --entry=independent --until-pc=independent_done --timing --machine=spacemit-x100 %s | filecheck %s --check-prefix=INDEPENDENT
# RUN: isasim --march=rv64im --entry=dependent --until-pc=dependent_done --timing --machine=spacemit-x100 %s | filecheck %s --check-prefix=DEPENDENT

.global independent_done
independent_done:
  add x0, x0, x0
.global independent
independent:
  addi x1, x0, 1
  div  x10, x20, x1
  divu x11, x20, x1
  rem  x12, x20, x1
  remu x13, x20, x1
  div  x14, x20, x1
  divu x15, x20, x1
  rem  x16, x20, x1
  remu x17, x20, x1

.global dependent_done
dependent_done:
  add x0, x0, x0
.global dependent
dependent:
  addi x1, x0, 1
  div  x10, x10, x1
  divu x10, x10, x1
  rem  x10, x10, x1
  remu x10, x10, x1
  div  x10, x10, x1
  divu x10, x10, x1
  rem  x10, x10, x1
  remu x10, x10, x1

# INDEPENDENT: 9 instructions, 27 cycles
# DEPENDENT: 9 instructions, 34 cycles
