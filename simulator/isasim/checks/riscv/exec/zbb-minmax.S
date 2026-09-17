# RUN: isasim --march=rv32i_zbb --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s --check-prefixes=CHECK,RV32
# RUN: isasim --march=rv64i_zbb --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s --check-prefixes=CHECK,RV64

.global done
done:
    add x0, x0, x0
.global _start
_start:
    addi x1, x0, -1
    addi x2, x0, 1
    min x3, x1, x2
    max x4, x1, x2
    minu x5, x1, x2
    maxu x6, x1, x2
    min x7, x2, x1
    max x8, x2, x1
    minu x9, x2, x1
    maxu x10, x2, x1
    min x11, x0, x0
    max x12, x1, x1
    minu x13, x0, x1
    maxu x14, x1, x0

# CHECK: final registers:
# RV32-DAG: GPR[3] = 0xffffffff (width=32)
# RV64-DAG: GPR[3] = 0xffffffffffffffff (width=64)
# CHECK-DAG: GPR[4] = 0x1
# CHECK-DAG: GPR[5] = 0x1
# RV32-DAG: GPR[6] = 0xffffffff (width=32)
# RV64-DAG: GPR[6] = 0xffffffffffffffff (width=64)
# RV32-DAG: GPR[7] = 0xffffffff (width=32)
# RV64-DAG: GPR[7] = 0xffffffffffffffff (width=64)
# CHECK-DAG: GPR[8] = 0x1
# CHECK-DAG: GPR[9] = 0x1
# RV32-DAG: GPR[10] = 0xffffffff (width=32)
# RV64-DAG: GPR[10] = 0xffffffffffffffff (width=64)
# CHECK-DAG: GPR[11] = 0x0
# RV32-DAG: GPR[12] = 0xffffffff (width=32)
# RV64-DAG: GPR[12] = 0xffffffffffffffff (width=64)
# CHECK-DAG: GPR[13] = 0x0
# RV32-DAG: GPR[14] = 0xffffffff (width=32)
# RV64-DAG: GPR[14] = 0xffffffffffffffff (width=64)
