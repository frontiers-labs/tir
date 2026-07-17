# Hardware model checking

`tir model-check` compares a hardware implementation with the instruction
semantics selected by a registered TIR target:

```sh
tir model-check --target=rv64g dut.btor2
```

The DUT must already be lowered to BTOR2. The command emits a TMDL reference
checker, connects it to the DUT by signal name, writes the combined model to
`target/model-check/<target>/miter.btor2` under the current directory, and runs
the external `btormc` engine. The instruction definitions are embedded in the
`tir` binary; no source checkout or separately installed TMDL files are needed.

## Retirement interface

The DUT must expose the architecture-neutral BTOR2 retirement outputs below.
The checker only requests as many `srcN_val` slots as the selected target needs.

| Output | Width | Meaning |
| --- | ---: | --- |
| `valid` | 1 | A modeled instruction retired. |
| `insn` | target encoding width | Retired instruction, zero-extended for shorter encodings. |
| `pc` | XLEN | Address of the retired instruction. |
| `src0_val`, `src1_val`, ... | XLEN | Integer source values, ordered by the source operands' TMDL declaration order. |
| `dst_addr` | target register-index width | Integer destination register index. |
| `dst_we` | 1 | Architectural integer destination write enable. |
| `dst_val` | XLEN | Value written through the destination view, zero-extended when narrower than XLEN. |
| `next_pc` | XLEN | Address of the next retired instruction. |

The reference side decodes `insn` and computes the expected destination write
and next PC from `pc` and the ordered source values. It emits separate properties
for write-enable, destination index, destination value, and next-PC mismatches.
Only decoded integer instructions whose TMDL behavior is expressible without
memory, traps, or non-integer register state and fits the single-destination
retirement contract enter the checker. `dst_we` must be false for an attempted
write discarded by a hardwired-zero destination.

If the DUT has an input named `reset`, the composed model constrains it high on
the first step and low afterward. Mismatch properties remain disabled until
reset has deasserted.

## Results

`btormc` reports `sat` when it finds a counterexample. Its witness contains the
retired instruction and values that disagree with TMDL. An `unsat` result only
covers the explored bound; use an induction-capable model checker and suitable
invariants for an unbounded proof.
