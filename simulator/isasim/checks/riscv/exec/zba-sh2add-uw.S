# RUN: isasim --march=rv64i_zba --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s --check-prefixes=CHECK,RV64

.global done
done:
    add x0, x0, x0
.global _start
_start:
    lui x1, 524288
    addi x2, x0, 1
    sh2add.uw x3, x1, x2
    addi x4, x0, -1
    sh2add.uw x5, x4, x4
    sh2add.uw x4, x4, x2
    sh2add.uw x2, x2, x2
    sh2add.uw x0, x1, x2
    addi x8, x0, 0
    sh2add.uw x6, x0, x2
    sh2add.uw x7, x2, x0
    addi x9, x0, -1
    sh2add.uw x9, x2, x9

# CHECK: final registers:
# RV64-DAG: GPR[8] = 0x0 (width=64)
# RV64-DAG: GPR[2] = 0x5 (width=64)
# RV64-DAG: GPR[6] = 0x5 (width=64)
# RV64-DAG: GPR[7] = 0x14 (width=64)
# RV64-DAG: GPR[3] = 0x200000001 (width=64)
# RV64-DAG: GPR[4] = 0x3fffffffd (width=64)
# RV64-DAG: GPR[5] = 0x3fffffffb (width=64)
# RV64-DAG: GPR[9] = 0x13 (width=64)
