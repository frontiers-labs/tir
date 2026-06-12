# SMT equivalence checking against the Sail RISC-V model

`cargo xtask verify-smt` proves, per instruction and per concrete operand
assignment, that the TMDL behavior and the [Sail RISC-V
model](https://github.com/riscv/sail-riscv) compute the same architectural
state — for **all** 2^64 values of every register, not just sampled ones. It
needs no hand-written tests and no hand-written proofs, so it is suitable for
nightly runs: `unsat` from the solver is a proof of agreement, `sat` is a
concrete counterexample.

## How it works

For each supported instruction and each operand tuple from a fixed boundary
set (x0 corner cases, register aliasing, immediate extremes):

1. The 32-bit instruction word is computed by evaluating the TMDL-generated
   `encode_*` function with z3, so encoding bugs surface as Sail decoding the
   word differently (or not at all).
2. [`isla-footprint`](https://github.com/rems-project/isla) symbolically
   executes that word in the Sail model over a fully symbolic register state.
   Each execution path yields a trace of register reads/writes plus SMT
   definitions and path constraints.
3. For every path, a z3 query asserts: initial states agree, the path
   constraints hold, and the final states (x1..x31 and the PC) differ. `unsat`
   proves equivalence on that path; `sat` prints which registers and values
   expose the divergence. The query files are left in
   `target/verify/smt/queries/` for inspection.

Sail traces are cached in `target/verify/smt/cache/` keyed by instruction
word; delete the directory after updating the Sail snapshot.

## Modeling assumptions

Reported with the results, and deliberate:

- Machine mode, no traps: Sail paths that touch state outside x-registers and
  the PC (CSRs, `mcause`, ...) are excluded and counted. TMDL behaviors do not
  model traps.
- The initial PC is 4-byte aligned and `nextPC = PC + 4` — the fetch invariant
  for non-compressed instructions. Together with 4-aligned branch immediates
  this makes Sail's misaligned-fetch trap paths vacuous.
- TMDL leaves the PC unchanged for fall-through instructions, so a Sail path
  that does not write `nextPC` requires TMDL's final PC to equal the initial
  PC; a path that writes it requires equality with the written value.
- Instructions whose behavior cannot be expressed in the SMT model (memory
  accesses, `trap(...)`) are marked `UNSUPPORTED-BEHAVIOR` in the generated
  SMT-LIB and reported as skipped. Memory instructions remain covered by the
  differential ISA test suite.

## Setup

Three external pieces are required:

- `z3` (the binary; `apt install z3`),
- `isla-footprint` ≥ the current `rems-project/isla` master, built with
  `cargo build --release` (needs `libz3-dev` or `Z3_SYS_Z3_HEADER`),
- a Sail RISC-V snapshot and isla config, e.g. `rv64d.ir` from
  [isla-snapshots](https://github.com/rems-project/isla-snapshots) and
  `configs/riscv64.toml` from the isla repository.

Point the harness at them:

```sh
export TIR_ISLA_FOOTPRINT=/path/to/isla/target/release/isla-footprint
export TIR_ISLA_SNAPSHOT=/path/to/isla-snapshots/rv64d.ir
export TIR_ISLA_CONFIG=/path/to/isla/configs/riscv64.toml
export TIR_Z3=z3
cargo xtask verify-smt
```

`TIR_VERIFY_SMT_FILTER=add,brancheq` restricts the run to selected
instructions. The isla config expects a `riscv64-linux-gnu-*` binutils
toolchain on `PATH`; only its presence is checked when concrete opcodes are
used, so stubs are sufficient.

## Reading the output

One line per instruction, one character per checked path: `.` proven
equivalent, `X` divergence (counterexample printed below the summary), `-`
excluded trap/system path, `E` no Sail execution path (the word is likely
illegal — an encoding bug), `I` isla failed or timed out on the word, `?`
solver timeout.
