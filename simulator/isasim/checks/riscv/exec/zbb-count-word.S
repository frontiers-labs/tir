# RUN: isasim --march=rv64i_zbb --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s

.global done
done:
    add x0, x0, x0
.global _start
_start:
    addi x1, x0, 1
    slli x1, x1, 32
    clzw x3, x1
    ctzw x4, x1
    cpopw x5, x1
    addi x2, x0, -1
    clzw x6, x2
    ctzw x7, x2
    cpopw x8, x2
    lui x9, 524288
    clzw x12, x9
    ctzw x13, x9
    cpopw x14, x9
    addi x15, x0, 40
    clzw x16, x15
    ctzw x17, x15
    cpopw x18, x15

# CHECK: final registers:
# CHECK-DAG: GPR[3] = 0x20 (
# CHECK-DAG: GPR[4] = 0x20 (
# CHECK-DAG: GPR[5] = 0x0 (
# CHECK-DAG: GPR[6] = 0x0 (
# CHECK-DAG: GPR[7] = 0x0 (
# CHECK-DAG: GPR[8] = 0x20 (
# CHECK-DAG: GPR[12] = 0x0 (
# CHECK-DAG: GPR[13] = 0x1f (
# CHECK-DAG: GPR[14] = 0x1 (
# CHECK-DAG: GPR[16] = 0x1a (
# CHECK-DAG: GPR[17] = 0x3 (
# CHECK-DAG: GPR[18] = 0x2 (
