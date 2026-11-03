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
- `libs/symbolic/src/ir-schema.egg` for authoritative relation names, sorts, invariants and index schedules.
- `tir/checks/Inputs/egglog-batch.egg` for an executable imported-IR example.
- `libs/egglog/src/ast.rs` and `libs/egglog/src/grammar.rs` for additional syntax. This is TIR's engine; verify unfamiliar syntax locally.

Build `cargo build -p tir-cli` when the binary is missing or stale. Check `target/debug/tir egglog --help`. The command takes a TIR file, followed by an optional Egglog file.

```sh
target/debug/tir egglog input.tir pass.egg
target/debug/tir egglog input.tir
```

The second form keeps imported facts, declarations and previous successful commands in one REPL session. Multiline commands work. Save the experiment as a batch file before reporting it: REPL errors print diagnostics and execution continues, so its exit status alone does not establish success. Restart after a failed mutation; do not assume rollback.

## Express the pass as rules

State the decision and its observable answer before writing rules. Choose a small checked-in TIR input that exercises the relevant operations. Add a reduced input only when existing examples cannot expose the distinction. Do not replace imported IR with manually seeded facts that assume the result.

Use the existing database and schema. Add pass-specific relations and named rulesets. Express matching, recursive analyses and candidate decisions as joins and rules. Do not implement graph walks, per-operation stepping or host-language copies of the IR to supply the answers.

Select the intended scope explicitly. This minimal batch selects module roots and demonstrates the index/query schedule:

```lisp
(ruleset prototype-select)
(rule ((= (OpKind op) "builtin.module"))
      ((Scope op) (SelectedScope op)) :ruleset prototype-select)
(run-schedule (saturate prototype-select)
              (saturate ir-index)
              (saturate ir-query))
(check (SelectedOp root root))
(print-function SelectedOp)
```

`Scope` requests containment materialization; `SelectedScope` requests selection over it. `Within` includes the root and nested functions. Use `OwningFunction` when the pass must stop at nested function boundaries. Run the required indexes after establishing scope, before consuming their derived views.

Preserve the schema's distinctions:

- Entity IDs are `i64` data. Value IDs and operation IDs are distinct; connect them through `ValueDefinition`, `Result` and `Operand`.
- RVSDG `InGraph` placement is unordered. CFG `InBlock` order is semantic; layout metadata does not define control flow.
- Structural type and attribute constructors are not semantic equivalence classes. Do not union or rewrite imported structural terms. Use separate semantic terms or proposal relations for transformations.
- The schema models one immutable IR revision. Start a fresh invocation for each input revision or independent alternative. Do not assume deleting a base fact retracts derived facts.

## Observe and decide

Run the saved batch on the real CLI. Use `check` for expected answers and print the candidate relation for inspection. Include a case that must not match when it distinguishes the approaches. Derive expected behavior from the pass contract, not from the rules under examination. Avoid assertions about allocated numeric IDs.

When comparing designs, use the same inputs in fresh processes and keep the variants separately runnable. Report failures and unsupported cases. A candidate relation proves the algorithm produced those candidates; it does not prove that IR edits, verification or compiled execution work.

The CLI writes answers to stdout and `egglog: compile=...s exec=...s` to stderr. These durations exclude IR import, source-rule setup, user-source parsing and output rendering. Compilation includes rule/query/action/schedule planning; execution covers the remaining command processing. Do not call them whole-pass or compiler timings. For performance decisions, use the repository's benchmark workflow and load the `benchmark-checklist` skill.

Deliver the decision, exact replay command, scratch path, observed answers and remaining integration limits. Preserve the runnable artifact. Do not promote the scratch implementation into production without the requested integration and its acceptance gates.
