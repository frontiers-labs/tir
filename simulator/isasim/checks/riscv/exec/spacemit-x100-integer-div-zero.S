# RUN: isasim --march=rv64im --entry=independent --until-pc=independent_done --timing --machine=spacemit-x100 %s | filecheck %s --check-prefix=INDEPENDENT
# RUN: isasim --march=rv64im --entry=dependent --until-pc=dependent_done --timing --machine=spacemit-x100 %s | filecheck %s --check-prefix=DEPENDENT

.global independent_done
independent_done:
  add x0, x0, x0
.global independent
independent:
  div  x10, x20, x0
  divu x11, x20, x0
  rem  x12, x20, x0
  remu x13, x20, x0
  div  x14, x20, x0
  divu x15, x20, x0
  rem  x16, x20, x0
  remu x17, x20, x0

.global dependent_done
dependent_done:
  add x0, x0, x0
.global dependent
dependent:
  div  x10, x10, x0
  divu x10, x10, x0
  rem  x10, x10, x0
  remu x10, x10, x0
  div  x10, x10, x0
  divu x10, x10, x0
  rem  x10, x10, x0
  remu x10, x10, x0

# INDEPENDENT: 8 instructions, 34 cycles
# DEPENDENT: 8 instructions, 41 cycles
