# Cycle-Approximate Simulation

TIR ships two performance tools built on one stack:

- **`isasim`** — a dynamic simulator. Executes a program and reports a
  cycle-approximate timing estimate for what actually ran.
- **`tir sched`** — a static throughput analyzer (llvm-mca style). Never
  executes anything; repeats a code region through a pipeline model and
  reports steady-state throughput, resource pressure, and timelines.

Both sit deliberately between `llvm-mca` and `gem5`: more honest than a static
analyzer (real execution, real branch outcomes, real addresses), far cheaper
than a microarchitecturally faithful simulator. The goal is quick
hardware/compiler experiments — "what does this loop cost if I double the ROB
or swap the predictor?" — not validated cycle accuracy.

## The three-layer split

The architecture follows one principle: *TMDL describes the device, Rust
describes the dynamics.*

| Layer | Where | What it owns |
|---|---|---|
| Instruction semantics | TMDL `behavior` | What an instruction does: register/memory/PC effects. Drives the functional executor (and isel — same single source of truth). |
| Performance model | TMDL `unit`/`machine`/`schedule` | What the device offers (functional units, issue width, buffers, pipeline phases, forwarding) and what each instruction consumes (latency, rthroughput, units). Compiled to a `MachineModel` per machine. |
| Microarchitecture dynamics | Rust (`tir-sim`) | How resources are scheduled over time: in-order vs. OOO windows, branch predictors, (future) caches and prefetchers. Swappable policies — the experiment surface. |

Adding an instruction to TMDL (semantics + `schedule` block) yields a working
compiler *and* simulator with no Rust changes. Sweeping a microarchitecture
knob is a Rust-side change (often just a CLI flag) with no TMDL changes.

The key TMDL design decision: **the instruction owns its identity, the machine
owns its cost**. Instructions join machine-independent `unit`s; each `machine`
binds those units to concrete latencies/resources (with per-instruction
`override`s). The same instruction can be cheap on one core and expensive on
another without touching the instruction definition.

## Crate map

```
simulator/simcore   (tir-sim)   the simulation library
├── program.rs      ProgramImage: asm module → addressed MachineBlocks
├── executor.rs     functional executor (architectural oracle) + trace recorder
├── scoreboard.rs   shared cycle-assignment engine + Prf + TimingConfig
├── timing.rs       trace-replay adapter over the scoreboard
├── predictor.rs    BranchPredictor trait + static policies (not-taken, BTFN)
└── error.rs

simulator/isasim    (tir-isasim) the dynamic CLI
├── main.rs         arg parsing, target selection, run orchestration
├── memory.rs       JSON memory configs + default snippet-test memory
└── dump.rs         --dump-state JSON snapshots (differential ISA testing)

tools/src/sched     `tir sched`: static analysis front-end + report views
backends/targets    --march/--mcpu/--mattr registry (TargetMachine per backend)
```

## Execution model: functional-first, timing-directed

`isasim` deliberately decouples *what happened* from *how long it took*:

1. **Program build.** The target's asm parser produces an IR module; `ProgramImage::from_module` lays each symbol out as a `MachineBlock` at
   consecutive addresses. The **block is the unit of fetch**: control
   transfers always land on block starts.
2. **Functional run.** The `Executor` interprets blocks straight-line via the
   TMDL-generated `MachineInstruction::execute`, maintaining only
   architectural state (registers keyed by physical file, flat memory, PC).
   An explicit PC write ends the block; otherwise execution falls through.
   With timing requested, it records the dynamic trace as `(OpId, pc)` pairs.
3. **Timing replay.** `timing::simulate` re-resolves each trace entry to its
   scheduling class and register defs/uses, recovers every branch outcome from
   consecutive PCs (plus a minimal BTB so not-taken branches have a target to
   predict against), and feeds the stream to the scoreboard.

Control-flow kind is not declared anywhere: it is derived at TMDL-compile time
from the behavior's `PC::pc` writes (`MachineInstruction::control_flow()`). A
guarded PC write is a `Conditional` branch and gets predictor-scored; an
unguarded one (`jal`, `bl`, `ret`, …) is `Unconditional` — its target is known
at decode, so it flows through the scoreboard as an ordinary instruction with
its scheduled resource and latency.

Why decoupled? The functional pass is a trustworthy oracle (differentially
tested against Spike via `cargo xtask isa-test-suite`), the timing model can
be iterated without touching execution, and the block-wise functional loop
leaves the door open to JIT-compiling hot blocks for fast functional mode
while timing still replays the recorded trace. The cost is that timing cannot
yet affect the executed path (no wrong-path fetch effects) — see
*Known gaps*.

## The scoreboard

`scoreboard::run` is the single cycle-assignment engine behind both tools, so
the static and dynamic views can never disagree about an instruction's cost.
It is a one-pass scoreboard, not an event-driven core model: for each
instruction in the (possibly repeated) stream it computes

- **dispatch** — in-order, ≤ issue-width per cycle, bounded by the ROB window
  (`rob` buffer from TMDL, overridable in `TimingConfig`), stalled by
  mispredict redirects and by physical-register-file pressure (`Prf`) on a
  renaming core;
- **issue** — when operands are ready (forwarding-aware producer→consumer
  latency via the machine's `forward` paths) and a lane of every required
  functional unit is free (reserved for `rthroughput` cycles); in-order cores
  additionally serialize on the previous issue;
- **retire** — issue + latency, in order.

Branch outcomes (dynamic mode only) are scored against a `BranchPredictor`; a
wrong guess stalls dispatch until the branch resolves plus a refetch penalty
(approximated from the TMDL `pipeline` depth). Report views (`tir sched
--view resource|timeline`) hang off the engine's `EventHandler` hooks; new
reports are new handlers, not engine changes.

The two callers differ only in how they construct the instruction stream:

| | `tir sched` (static) | `isasim --timing` (dynamic) |
|---|---|---|
| Stream | region × `--iterations` | recorded execution trace |
| Branches | invisible (no outcomes) | resolved, predictor-scored |
| Dependencies | reconstructed from phys regs | same |
| Result | report views | one summary line |

## CLI conventions (snippet mode)

Today's input is a bare `.S` snippet, not an ELF. The conventions exist to
make small tests writable:

- `--until-pc <symbol|addr>` stops execution (no exit syscall yet); a `done`
  label is the usual sentinel.
- The asm parser emits symbols in **reverse source order**, so test files
  declare `done` *before* the entry label to place it after the entry in
  memory.
- Without `--memory-config`, a deterministic test allocation is installed and
  (RISC-V only) `a0`/`a1` point into it (`memory.rs`).
- `--dump-state`/`--dump-mem` emit a JSON architectural snapshot for the
  differential ISA suite.

## Known gaps

- Indirect transfers (`jalr`/`br`/`ret`) are costed like direct ones: no
  BTB/RAS modeling, so an indirect-target mispredict is never charged.
- No cache or memory-latency modeling: loads cost their scheduled latency
  regardless of address, although the trace already carries real addresses.
- Timing is replay-only — no wrong-path effects, no speculative state.
- Memory is one flat region at `--mem-start-address`; no permissions, no
  sparse segments.
- The scoreboard retires strictly in order at issue+latency; writeback
  buffers, store queues, and `lsq`/`iq` buffer sizes declared in TMDL are not
  yet consumed.

## Roadmap

1. **Indirect-branch prediction** — BTB + return-address stack policies in
   `predictor.rs`, scored on `Unconditional` transfers whose target comes from
   a register.
2. **Caches & prefetchers** — swappable Rust policies (set-associative, RRIP,
   stride prefetch) fed by the trace's real addresses; stall attribution in
   the report.
3. **Syscall-emulation mode** — load static ELF executables (segments →
   sparse memory regions, entry from the header), emulate a small Linux
   syscall surface (`exit`, `write`, `brk`, …) so real `main()`s run; replaces
   the `--until-pc` sentinel and the default-memory hack.
4. **Fast functional mode** — JIT or threaded-interpret hot blocks
   (`MachineBlock` already keeps the IR block handle); timing remains
   trace-driven.
5. **Coupled/speculative mode** — drive fetch from the timing model instead
   of replay once wrong-path effects matter.
