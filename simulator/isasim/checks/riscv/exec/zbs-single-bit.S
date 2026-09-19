# RUN: isasim --march=rv32i_zbs --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s --check-prefixes=CHECK,RV32
# RUN: isasim --march=rv64i_zbs --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s --check-prefixes=CHECK,RV64

.global done
done:
    add x0, x0, x0
.global _start
_start:
    addi x1, x0, -1
    addi x2, x0, 0
    addi x6, x0, 5
    bset x3, x2, x6
    bclr x4, x1, x6
    binv x5, x2, x6
    bext x7, x1, x6
    bext x8, x2, x6
    bseti x9, x2, 31
    bclri x10, x1, 31
    binvi x11, x2, 31
    bexti x12, x1, 31
    addi x13, x0, 32
    bset x14, x2, x13
    addi x15, x0, 64
    bset x16, x2, x15

# CHECK: final registers:
# CHECK-DAG: GPR[3] = 0x20
# CHECK-DAG: GPR[5] = 0x20
# CHECK-DAG: GPR[7] = 0x1
# CHECK-DAG: GPR[8] = 0x0
# CHECK-DAG: GPR[9] = 0x80000000
# CHECK-DAG: GPR[11] = 0x80000000
# CHECK-DAG: GPR[12] = 0x1
# CHECK-DAG: GPR[16] = 0x1
# RV32-DAG: GPR[4] = 0xffffffdf (width=32)
# RV64-DAG: GPR[4] = 0xffffffffffffffdf (width=64)
# RV32-DAG: GPR[10] = 0x7fffffff (width=32)
# RV64-DAG: GPR[10] = 0xffffffff7fffffff (width=64)
# RV32-DAG: GPR[14] = 0x1 (width=32)
# RV64-DAG: GPR[14] = 0x100000000 (width=64)
