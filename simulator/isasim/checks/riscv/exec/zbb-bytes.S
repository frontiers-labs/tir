# RUN: isasim --march=rv32i_zbb --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s --check-prefixes=CHECK,RV32
# RUN: isasim --march=rv64i_zbb --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s --check-prefixes=CHECK,RV64

.global done
done:
    add x0, x0, x0
.global _start
_start:
    addi x1, x0, -128
    sext.b x3, x1
    sext.h x4, x1
    zext.h x5, x1
    lui x2, 8
    sext.h x6, x2
    sext.b x7, x2
    zext.h x8, x2
    addi x9, x0, 128
    rev8 x12, x9
    orc.b x13, x9
    lui x14, 4096
    addi x14, x14, 1
    orc.b x15, x14
    rev8 x16, x14
    rev8 x17, x16
    orc.b x18, x0
    addi x19, x0, -1
    orc.b x20, x19
    rev8 x21, x19
    sext.b x1, x1
    zext.h x2, x2

# CHECK: final registers:
# RV32-DAG: GPR[3] = 0xffffff80 (
# RV64-DAG: GPR[3] = 0xffffffffffffff80 (
# RV32-DAG: GPR[4] = 0xffffff80 (
# RV64-DAG: GPR[4] = 0xffffffffffffff80 (
# CHECK-DAG: GPR[5] = 0xff80 (
# RV32-DAG: GPR[6] = 0xffff8000 (
# RV64-DAG: GPR[6] = 0xffffffffffff8000 (
# CHECK-DAG: GPR[7] = 0x0 (
# CHECK-DAG: GPR[8] = 0x8000 (
# RV32-DAG: GPR[12] = 0x80000000 (
# RV64-DAG: GPR[12] = 0x8000000000000000 (
# CHECK-DAG: GPR[13] = 0xff (
# CHECK-DAG: GPR[15] = 0xff0000ff (
# RV32-DAG: GPR[16] = 0x1000001 (
# RV64-DAG: GPR[16] = 0x100000100000000 (
# CHECK-DAG: GPR[17] = 0x1000001 (
# CHECK-DAG: GPR[18] = 0x0 (
# RV32-DAG: GPR[20] = 0xffffffff (
# RV64-DAG: GPR[20] = 0xffffffffffffffff (
# RV32-DAG: GPR[21] = 0xffffffff (
# RV64-DAG: GPR[21] = 0xffffffffffffffff (
