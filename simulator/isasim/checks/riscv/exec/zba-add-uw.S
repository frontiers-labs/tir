# RUN: isasim --march=rv64i_zba --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s --check-prefixes=CHECK,RV64

.global done
done:
    add x0, x0, x0
.global _start
_start:
    lui x1, 524288
    addi x2, x0, 1
    add.uw x3, x1, x2
    addi x4, x0, -1
    add.uw x5, x4, x4
    add.uw x4, x4, x2
    add.uw x2, x2, x2
    add.uw x0, x1, x2
    addi x8, x0, 0
    add.uw x6, x0, x2
    add.uw x7, x2, x0
    addi x9, x0, -1
    add.uw x9, x2, x9

# CHECK: final registers:
# RV64-DAG: GPR[8] = 0x0 (width=64)
# RV64-DAG: GPR[2] = 0x2 (width=64)
# RV64-DAG: GPR[6] = 0x2 (width=64)
# RV64-DAG: GPR[7] = 0x2 (width=64)
# RV64-DAG: GPR[3] = 0x80000001 (width=64)
# RV64-DAG: GPR[4] = 0x100000000 (width=64)
# RV64-DAG: GPR[5] = 0xfffffffe (width=64)
# RV64-DAG: GPR[9] = 0x1 (width=64)
