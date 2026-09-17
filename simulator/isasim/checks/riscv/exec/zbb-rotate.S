# RUN: isasim --march=rv32i_zbb --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s --check-prefixes=CHECK,RV32
# RUN: isasim --march=rv64i_zbb --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s --check-prefixes=CHECK,RV64

.global done
done:
    add x0, x0, x0
.global _start
_start:
    addi x1, x0, 1
    addi x2, x0, -1
    ror x3, x1, x1
    rol x4, x3, x1
    rori x5, x1, 0
    rori x6, x1, 31
    ror x7, x1, x2
    rol x8, x1, x2
    ror x9, x2, x0
    rori x12, x2, 17
    addi x13, x0, 64
    ror x14, x1, x13
    rol x15, x1, x13
    rol x1, x1, x1
    ror x2, x2, x2

# CHECK: final registers:
# CHECK-DAG: GPR[1] = 0x2 (
# RV32-DAG: GPR[2] = 0xffffffff (
# RV64-DAG: GPR[2] = 0xffffffffffffffff (
# RV32-DAG: GPR[3] = 0x80000000 (
# RV64-DAG: GPR[3] = 0x8000000000000000 (
# CHECK-DAG: GPR[4] = 0x1 (
# CHECK-DAG: GPR[5] = 0x1 (
# RV32-DAG: GPR[6] = 0x2 (
# RV64-DAG: GPR[6] = 0x200000000 (
# CHECK-DAG: GPR[7] = 0x2 (
# RV32-DAG: GPR[8] = 0x80000000 (
# RV64-DAG: GPR[8] = 0x8000000000000000 (
# RV32-DAG: GPR[9] = 0xffffffff (
# RV64-DAG: GPR[9] = 0xffffffffffffffff (
# RV32-DAG: GPR[12] = 0xffffffff (
# RV64-DAG: GPR[12] = 0xffffffffffffffff (
# CHECK-DAG: GPR[14] = 0x1 (
# CHECK-DAG: GPR[15] = 0x1 (
