# RUN: isasim --march=rv64i_zba --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s --check-prefixes=CHECK,RV64

.global done
done:
    add x0, x0, x0
.global _start
_start:
    lui x1, 524288
    addi x2, x0, 1
    slli.uw x3, x1, 1
    addi x4, x0, -1
    slli.uw x5, x4, 3
    slli.uw x4, x4, 0
    slli.uw x2, x2, 0
    slli.uw x0, x1, 1
    addi x8, x0, 0
    slli.uw x6, x0, 31
    slli.uw x7, x2, 0
    slli.uw x9, x4, 63

# CHECK: final registers:
# RV64-DAG: GPR[8] = 0x0 (width=64)
# RV64-DAG: GPR[2] = 0x1 (width=64)
# RV64-DAG: GPR[6] = 0x0 (width=64)
# RV64-DAG: GPR[7] = 0x1 (width=64)
# RV64-DAG: GPR[3] = 0x100000000 (width=64)
# RV64-DAG: GPR[4] = 0xffffffff (width=64)
# RV64-DAG: GPR[5] = 0x7fffffff8 (width=64)
# RV64-DAG: GPR[9] = 0x8000000000000000 (width=64)
