# RUN: isasim --march=rv32i_zbb --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s --check-prefixes=CHECK,RV32
# RUN: isasim --march=rv64i_zbb --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s --check-prefixes=CHECK,RV64

.global done
done:
    add x0, x0, x0
.global _start
_start:
    addi x1, x0, 12
    addi x2, x0, 10
    addi x10, x0, -1
    andn x3, x1, x2
    orn x4, x1, x2
    xnor x5, x1, x2
    andn x6, x10, x10
    orn x7, x0, x0
    xnor x8, x10, x0
    andn x9, x10, x0
    orn x11, x0, x10
    xnor x12, x0, x0

# CHECK: final registers:
# CHECK-DAG: GPR[3] = 0x4
# RV32-DAG: GPR[4] = 0xfffffffd (width=32)
# RV64-DAG: GPR[4] = 0xfffffffffffffffd (width=64)
# RV32-DAG: GPR[5] = 0xfffffff9 (width=32)
# RV64-DAG: GPR[5] = 0xfffffffffffffff9 (width=64)
# CHECK-DAG: GPR[6] = 0x0
# RV32-DAG: GPR[7] = 0xffffffff (width=32)
# RV64-DAG: GPR[7] = 0xffffffffffffffff (width=64)
# CHECK-DAG: GPR[8] = 0x0
# RV32-DAG: GPR[9] = 0xffffffff (width=32)
# RV64-DAG: GPR[9] = 0xffffffffffffffff (width=64)
# CHECK-DAG: GPR[11] = 0x0
# RV32-DAG: GPR[12] = 0xffffffff (width=32)
# RV64-DAG: GPR[12] = 0xffffffffffffffff (width=64)
