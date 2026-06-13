# BTOR2 hardware model checking against the TMDL model

`cargo xtask verify-btor2 <isa> <impl.btor2>` searches a hardware
implementation for instructions that diverge from the TMDL specification. Where
[SMT equivalence checking](./smt_verification.md) proves *spec vs spec* (TMDL ≡
Sail), this checks *implementation vs spec* (RTL ≡ TMDL) with a word-level model
checker. Because TMDL is already proven against Sail, agreement with TMDL
transitively relates the hardware to the canonical model.

## Why a checker, not a transition-system diff

A pipelined core and a single-step ISA model cannot be compared cycle by cycle.
The flow follows [riscv-formal](https://github.com/SymbioticEDA/riscv-formal):
the implementation exposes a **retirement interface** — for each committed
instruction it reports what it did — and a *combinational* reference checker
recomputes the architectural result and asserts they agree. Timing is decoupled
from semantics, so the pipeline depth, forwarding and hazards are all in scope
without modeling the microarchitecture.

`tmdlc --action=emit-btor2 --isa=<ISA>` generates that checker
(`tmdl/src/btor2gen.rs`). It is purely combinational over the retirement
signals and ends in one `bad` property: the mismatch.

## The retirement interface

The implementation's BTOR2 must expose these as **outputs**, named exactly. The
stitcher wires them into the checker by name (`xtask/src/verify_btor2.rs`,
`RVFI_SIGNALS`):

| Signal     | Width  | Meaning                                                        |
|------------|--------|---------------------------------------------------------------|
| `valid`    | 1      | A modeled instruction retired this step.                      |
| `insn`     | 32     | The retired instruction word.                                 |
| `pc`       | XLEN   | Its program counter.                                          |
| `rs1_val`  | XLEN   | Value read for the `rs1` source.                              |
| `rs2_val`  | XLEN   | Value read for the `rs2` source.                              |
| `rd_addr`  | 5      | Destination register index it wrote.                          |
| `rd_we`    | 1      | Whether it wrote the integer register file.                   |
| `rd_val`   | XLEN   | Value written to `rd`.                                        |
| `next_pc`  | XLEN   | PC of the next retired instruction (branch target or pc + 4). |

The checker computes the golden `rd_we`/`rd_val`/`rd_addr`/`next_pc` from
`insn`, `pc`, `rs1_val`, `rs2_val` and raises `bad` when, on a `valid` step,
any of them disagree. Reads come from the reported source *values*, so the
implementation's register file and forwarding are exercised, not re-modeled.

## Scope

Matches `verify-smt`: register-only instructions (RV32I/RV64I arithmetic,
logic, shifts, comparisons, branches, jumps, LUI/AUIPC, and the M extension).
Behaviors touching memory or traps are dropped from the dispatch, so loads,
stores, CSRs and exceptions are out of scope. The property only fires on
decoded, modeled instructions: an unmodeled retired instruction is ignored, not
falsely flagged.

## End-to-end run

1. **Lower the implementation to BTOR2.** From a Chisel design (e.g. Svarog),
   elaborate a formal top exposing the retirement outputs, then run Yosys:

   ```sh
   # Chisel -> Verilog (Svarog ships this generator; firtool is fetched by Chisel)
   ./mill svarog.runMain svarog.formal.FormalGenerator \
       --config=configs/svg-micro.yaml --target-dir=out/formal

   # Verilog -> BTOR2 (the writer is `write_btor`, which emits BTOR2)
   yosys -p 'read_verilog -sv out/formal/FormalTop.sv; prep -top FormalTop; \
            flatten; setundef -zero; write_btor impl.btor2'
   ```

   The generator passes the firtool lowering options
   `disallowLocalVariables,disallowPackedArrays` and
   `--default-layer-specialization=enable` so the emitted SystemVerilog stays
   within the Yosys Verilog frontend. Svarog's `FormalTop` names the outputs to
   match the table above.

2. **Generate, stitch and check.**

   ```sh
   cargo xtask verify-btor2 riscv32 impl.btor2
   ```

   This emits the checker, writes the miter to `target/verify/btor2/miter.btor2`,
   and — if `btormc` (Boolector/Bitwuzla) is on `PATH` — runs it.

3. **Run the checker manually** if you prefer a different engine or bound:

   ```sh
   btormc -kmax 20 target/verify/btor2/miter.btor2
   pono --engine bmc target/verify/btor2/miter.btor2
   ```

## Reading the result

`unsat` (up to the bound) means no divergence was found. `sat` is a
counterexample: the witness assigns the `insn`, `pc` and source values that
expose the bug, and the implementation outputs that disagree with the spec.
Decode `insn` to identify the instruction and compare `rd_val`/`next_pc`
against the spec to see which field is wrong.

The stitcher drives a one-cycle reset pulse (constraining the design's `reset`
input) and gates the property until reset deasserts, so uninitialized pipeline
state at step 0 cannot raise a spurious counterexample.

## Constraining the memory interface

`FormalTop` leaves the instruction/data memory responses as free inputs. That
suffices to explore every fetched word, but with no protocol constraint the
fetch unit can ingest phantom responses (a `resp.valid` with no outstanding
request), which produce retirements that no real memory would and so spurious
counterexamples. Before trusting a `sat`, constrain the memory to follow the
request/response handshake and return a stable word per address (the
riscv-formal "memory abstraction"). As a sanity check, forcing the
instruction-response valid bits low makes the model `unsat`: no instruction
retires, confirming such counterexamples are interface artifacts, not core
bugs.

## Limitations

- **Bounded.** BMC finds bugs up to depth *k*; it is not a proof of correctness.
  For unbounded guarantees use k-induction (Pono), which needs invariants for
  pipelines. For bug-finding, BMC is the right tool.
- **Register-only**, as above. Memory and CSR checking need a shared-memory
  miter and are not implemented.
- The implementation must honor the retirement contract faithfully; a buggy
  retirement interface can mask or fabricate divergences.
