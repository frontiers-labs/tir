# RUN: isasim --march=rv64iv --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s
# RUN: not isasim --march=rv64i --entry=_start --until-pc=done %s

# vset{i}vli configure the vector unit: vl gets the granted element count
# (min(AVL, VLMAX), with VLMAX = VLEN * LMUL / SEW and VLEN = 128), vtype the
# packed vtypei immediate, and the decomposed VCFG::sew field the OPIVV
# behaviors read. vsetvli also returns the grant in rd. 200 = 0xc8 packs
# e16/m1/ta/ma; 208 = 0xd0 packs e32/m1/ta/ma, so AVL = 7 is clamped to
# VLMAX = 4. The vadd.vv executes under the configured vl/sew (sources read as
# zero; the simulator's register file holds up to 64 bits per register, so
# vl*sew <= 64).
.global done
done:
  add x0, x0, x0
.global _start
_start:
  vsetivli x0, 4, 200
  vadd.vv v1, v2, v3
  addi x1, x0, 7
  vsetvli x5, x1, 208

# CHECK: final registers:
# CHECK-DAG: GPR[5] = 0x4
# CHECK-DAG: VCSR[3104] = 0x4
# CHECK-DAG: VCSR[3105] = 0xd0
# CHECK-DAG: VCFG[0] = 0x20
