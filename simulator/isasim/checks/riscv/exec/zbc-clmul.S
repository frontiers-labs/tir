# RUN: isasim --march=rv32i_zbc --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s --check-prefixes=CHECK,RV32
# RUN: isasim --march=rv64i_zbc --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s --check-prefixes=CHECK,RV64

.global done
done:
    add x0, x0, x0
.global _start
_start:
    addi x1, x0, 3
    addi x2, x0, 5
    clmul x3, x1, x2
    clmulh x4, x1, x2
    clmulr x5, x1, x2
    addi x6, x0, -1
    addi x7, x0, -1
    clmul x8, x6, x7
    clmulh x9, x6, x7
    clmulr x10, x6, x7

# CHECK: final registers:
# CHECK-DAG: GPR[3] = 0xf
# CHECK-DAG: GPR[4] = 0x0
# CHECK-DAG: GPR[5] = 0x0
# RV32-DAG: GPR[8] = 0x55555555 (width=32)
# RV64-DAG: GPR[8] = 0x5555555555555555 (width=64)
# RV32-DAG: GPR[9] = 0x55555555 (width=32)
# RV64-DAG: GPR[9] = 0x5555555555555555 (width=64)
# RV32-DAG: GPR[10] = 0xaaaaaaaa (width=32)
# RV64-DAG: GPR[10] = 0xaaaaaaaaaaaaaaaa (width=64)
