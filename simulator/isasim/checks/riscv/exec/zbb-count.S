# RUN: isasim --march=rv32i_zbb --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s --check-prefixes=CHECK,RV32
# RUN: isasim --march=rv64i_zbb --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s --check-prefixes=CHECK,RV64

.global done
done:
    add x0, x0, x0
.global _start
_start:
    addi x1, x0, 1
    addi x2, x0, -1
    clz x3, x0
    ctz x4, x0
    cpop x5, x0
    clz x6, x1
    ctz x7, x1
    cpop x8, x1
    clz x9, x2
    ctz x12, x2
    cpop x13, x2
    lui x14, 524288
    clz x15, x14
    ctz x16, x14
    cpop x17, x14
    addi x18, x0, 40
    clz x19, x18
    ctz x20, x18
    cpop x21, x18

# CHECK: final registers:
# RV32-DAG: GPR[3] = 0x20 (
# RV64-DAG: GPR[3] = 0x40 (
# RV32-DAG: GPR[4] = 0x20 (
# RV64-DAG: GPR[4] = 0x40 (
# CHECK-DAG: GPR[5] = 0x0 (
# RV32-DAG: GPR[6] = 0x1f (
# RV64-DAG: GPR[6] = 0x3f (
# CHECK-DAG: GPR[7] = 0x0 (
# CHECK-DAG: GPR[8] = 0x1 (
# CHECK-DAG: GPR[9] = 0x0 (
# CHECK-DAG: GPR[12] = 0x0 (
# RV32-DAG: GPR[13] = 0x20 (
# RV64-DAG: GPR[13] = 0x40 (
# CHECK-DAG: GPR[15] = 0x0 (
# CHECK-DAG: GPR[16] = 0x1f (
# RV32-DAG: GPR[17] = 0x1 (
# RV64-DAG: GPR[17] = 0x21 (
# RV32-DAG: GPR[19] = 0x1a (
# RV64-DAG: GPR[19] = 0x3a (
# CHECK-DAG: GPR[20] = 0x3 (
# CHECK-DAG: GPR[21] = 0x2 (
