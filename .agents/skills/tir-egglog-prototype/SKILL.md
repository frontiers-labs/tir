---
name: tir-egglog-prototype
description: Prototype compiler analyses and passes over imported TIR using the tir egglog batch command or persistent REPL. Use to test relational algorithms before production integration.
---

# Prototype a TIR pass with Egglog

Make a pass-design question executable against real imported IR. Keep the rules, inputs, commands and observed answers together in a scratch directory. Stop when the question is answered; production integration is a separate task unless requested.

## Start from the checkout

Run from the TIR repository root. Inspect local instructions and `git status` first.

Read these sources for the parts the prototype needs:

- `tir/src/egglog.rs` for CLI arguments, REPL behavior and timing boundaries.
- `core/defs/ir.cdsl` for relation names, sorts, invariants and index schedules.
- `tir/checks/Inputs/egglog-rules.cdsl` for an executable imported-IR example.

Build `cargo build -p tir-cli` when the binary is missing or stale. Check `target/debug/tir egglog --help`. The command takes a TIR file, followed by an optional CDSL file. Use `--schedule` to select a schedule other than `execute`.

```sh
target/debug/tir egglog input.tir pass.cdsl
target/debug/tir egglog input.tir
```

The second form keeps imported facts and accepted CDSL declarations in one session. It runs the selected schedule when available after each accepted revision. Save the experiment as a batch file before reporting it. A REPL diagnostic can leave the exit status successful. Restart after a failed mutation; do not assume rollback.

## Express the pass as rules

State the decision and its observable answer before writing rules. Choose a small checked-in TIR input that exercises the relevant operations. Add a reduced input only when existing examples cannot expose the distinction. Do not replace imported IR with manually seeded facts that assume the result.

Use the existing database and schema. Add pass-specific relations and named rulesets. Express matching, recursive analyses and candidate decisions as joins and rules. Do not implement graph walks, per-operation stepping or host-language copies of the IR to supply the answers.

Declare the IR relations used by the pass and expose a named schedule:

```cdsl
pub let Operation: (Int, String, String, Int, Int, Int, Int) -> Prop;
pub let Selected: Int -> Prop;
pub mod rules {
    pub let select<id, place, parent, position, order, location>:
        Operation(id, "builtin.module", place, parent, position, order, location) -> Selected(id);
}
pub let execute = std::schedule::saturate(rules);
```

Preserve the schema's distinctions:

- Entity IDs are `i64` data. Value IDs and operation IDs are distinct; connect them through `ValueDefinition`, `Result` and `Operand`.
- RVSDG `InGraph` placement is unordered. CFG `InBlock` order is semantic; layout metadata does not define control flow.
- Structural type and attribute constructors are not semantic equivalence classes. Do not union or rewrite imported structural terms. Use separate semantic terms or proposal relations for transformations.
- The schema models one immutable IR revision. Start a fresh invocation for each input revision or independent alternative. Do not assume deleting a base fact retracts derived facts.

## Observe and decide

Run the saved batch on the real CLI. The timing only confirms execution. For an observable result, inspect the derived relation through the database API in a focused probe. Include a case that must not match when it distinguishes the approaches. Derive expected behavior from the pass contract, not from the rules under examination. Avoid assertions about allocated numeric IDs.

When comparing designs, use the same inputs in fresh processes and keep the variants separately runnable. Report failures and unsupported cases. A candidate relation proves the algorithm produced those candidates; it does not prove that IR edits, verification or compiled execution work.

The CLI writes `egglog: compile=...s exec=...s` to stderr. These durations cover CDSL lowering, compilation and loading, then schedule execution. They exclude IR import and source parsing. For performance decisions, use the repository's benchmark workflow and load the `benchmark-checklist` skill.

Deliver the decision, exact replay command, scratch path, observed answers and remaining integration limits. Preserve the runnable artifact. Do not promote the scratch implementation into production without the requested integration and its acceptance gates.
