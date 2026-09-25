# SMT equivalence checking against Sail models

`cargo xtask verify <isa>` compares TMDL behavior with pinned Sail models. It
samples instruction operands and keeps architectural state symbolic. An `unsat`
result proves agreement for one execution path; `sat` gives a counterexample.
Excluded paths and solver unknowns remain visible in the report.

## How it works

For each supported instruction and each operand tuple from a fixed boundary
set (x0 corner cases, register aliasing, immediate extremes):

1. The instruction word is computed and decoded directly from TMDL's
   structured encoding fields, so encoding bugs surface as Sail decoding the
   word differently without a solver round-trip.
2. The pinned [`isla-lib`](https://github.com/rems-project/isla) dependency
   loads the snapshot once and symbolically executes a batch of words over a
   fully symbolic register state.
   Each execution path yields a trace of register reads/writes plus SMT
   definitions and path constraints.
3. For every path, an SMT query asks whether the models can reach different
   mapped architectural states from the same initial state under the path
   constraints. `unsat` proves agreement; `sat` supplies a counterexample.
   Queries are saved in `target/verify/smt/<isa>/queries/` for inspection.

Sail traces are cached in `target/verify/smt/<isa>/cache/`, keyed by instruction
word plus a fingerprint of the snapshot and isla config, so swapping either
invalidates the cache automatically.

## Modeling assumptions

Reported with the results, and deliberate:

- Machine mode, no unmodeled traps: Sail paths that touch architectural state
  without a TMDL mapping are excluded and counted.
- The initial PC is 4-byte aligned and `nextPC = PC + 4` — the fetch invariant
  for non-compressed instructions. Together with 4-aligned branch immediates
  this makes Sail's misaligned-fetch trap paths vacuous.
- TMDL leaves the PC unchanged for fall-through instructions, so a Sail path
  that does not write `nextPC` requires TMDL's final PC to equal the initial
  PC; a path that writes it requires equality with the written value.
- Only modeled, defined status flags are compared. The verifier corrects known
  flag errors in the pinned reference model from its execution trace.
- Instructions without SMT behavior or executable reference traces are
  reported as unsupported.

## Setup

External inputs are:

- `z3` and `libz3-dev`; z3 is the fallback solver and `libz3-dev` is required
  by the pinned `isla-lib` Rust dependency,
- Bitwuzla 0.9.1 or later is recommended as the primary solver,
- a Sail RISC-V snapshot, e.g. `rv64d.ir` from
  [isla-snapshots](https://github.com/rems-project/isla-snapshots).

The isla configurations live in `xtask/`. The RISC-V configurations disable C,
which the PC alignment assumptions rely on. Point the harness at the tools:

```sh
export TIR_ISLA_SNAPSHOT=/path/to/isla-snapshots/rv64d.ir
export TIR_BITWUZLA=bitwuzla
export TIR_Z3=z3
cargo xtask verify riscv64
```

`TIR_VERIFY_SMT_FILTER=add,brancheq` restricts the run to selected
instructions. `--shard k/N` selects a stable hash partition for CI or local
parallel runs. `TIR_VERIFY_SMT_ISLA_JOBS` controls the Isla library worker
pool (default: available CPU count). If `TIR_BITWUZLA` is unset and Bitwuzla is
not on `PATH`, the verifier uses z3 only. Each run writes per-instruction stage timings and
verified/excluded/unknown counts to
`target/verify/smt/<isa>/report.json`. The isla config expects a
`riscv64-linux-gnu-*` binutils
toolchain on `PATH`; only its presence is checked when concrete opcodes are
used, so stubs are sufficient.

## Generated metadata

`tmdlc --action=emit-smtlib --output=<name>.smt2` also writes
`<name>.metadata.json`. The versioned sidecar describes each instruction's
operands and the `#[align]`/`#[nonzero]` constraints they declare (the concrete
boundary cases stay inside them), encoding width, support status, flattened state expressions, PC and register-file writes,
reservation use, memory address terms, and trap kinds. The verifier reads this
sidecar directly; it does not recover behavior facts by scanning SMT function
bodies or substituting text in emitted expressions.

Behavior statements lower once to the target-independent `Effect` IR in
`sem_expr_state.rs`; Rust and SMT generation consume that same assignment,
control-flow, memory, reservation, fence, and trap structure. Value expressions
use `SemGraph`. Common scalar operators are defined in one table with arity,
width rule, concrete evaluation, SMT template, and Rust operation name; width
inference, the interpreter, and SMT generation consume that table.

## Reading the output

One line per instruction, one character per checked path: `.` proven
equivalent, `X` divergence (counterexample printed below the summary), `-`
excluded trap/system path, `E` no Sail execution path (the word is likely
illegal — an encoding bug), `I` isla failed or timed out on the word, `?`
solver timeout.
