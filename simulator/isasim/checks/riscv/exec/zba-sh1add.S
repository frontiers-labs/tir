# RUN: isasim --march=rv32i_zba --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s --check-prefixes=CHECK,RV32
# RUN: isasim --march=rv64i_zba --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s --check-prefixes=CHECK,RV64

.global done
done:
    add x0, x0, x0
.global _start
_start:
    lui x1, 524288
    addi x2, x0, 1
    sh1add x3, x1, x2
    addi x4, x0, -1
    sh1add x5, x4, x4
    sh1add x4, x4, x2
    sh1add x2, x2, x2
    sh1add x0, x1, x2
    addi x8, x0, 0
    sh1add x6, x0, x2
    sh1add x7, x2, x0
    addi x9, x0, -1
    sh1add x9, x2, x9

# CHECK: final registers:
# RV32-DAG: GPR[8] = 0x0 (width=32)
# RV32-DAG: GPR[2] = 0x3 (width=32)
# RV32-DAG: GPR[6] = 0x3 (width=32)
# RV32-DAG: GPR[7] = 0x6 (width=32)
# RV32-DAG: GPR[3] = 0x1 (width=32)
# RV32-DAG: GPR[4] = 0xffffffff (width=32)
# RV32-DAG: GPR[5] = 0xfffffffd (width=32)
# RV32-DAG: GPR[9] = 0x5 (width=32)
# RV64-DAG: GPR[8] = 0x0 (width=64)
# RV64-DAG: GPR[2] = 0x3 (width=64)
# RV64-DAG: GPR[6] = 0x3 (width=64)
# RV64-DAG: GPR[7] = 0x6 (width=64)
# RV64-DAG: GPR[3] = 0xffffffff00000001 (width=64)
# RV64-DAG: GPR[4] = 0xffffffffffffffff (width=64)
# RV64-DAG: GPR[5] = 0xfffffffffffffffd (width=64)
# RV64-DAG: GPR[9] = 0x5 (width=64)
