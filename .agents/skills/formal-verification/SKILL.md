---
name: formal-verification
description: "Use to formally verify parts of TIR crates logic (egglog, adt, etc)"
---

# Formal verification for TIR

Prove TIR's implementation. Verification of programs compiled by TIR is a
different task.

## Start from the production contract

Identify the actual Rust functions, the property to prove, caller preconditions,
and omitted dependencies. State what success means before choosing a harness or
theorem. A functional claim needs a functional postcondition; panic freedom alone
does not establish lookup, ordering, or mutation correctness.

- Use Hax/Lean for inductive invariants and properties over arbitrary valid
  collection sizes. First confirm the selected production code extracts.
- Use Kani for symbolic execution of the actual Rust implementation, including
  supported unsafe and SIMD operations. A naturally fixed domain can be covered
  completely; a chosen collection-size cutoff limits the claim to that cutoff.
- Use both when they cover useful complementary properties. Check actual tool
  support before promising a proof.

Selected production-function proofs under explicit dependency contracts are
useful. Do not require verification of every dependency before starting. Keep
the target implementation checked, and distinguish assumed library laws from
proved target behavior. Do not replace the target with a copied implementation,
an opaque declaration, a stub, or a proof-only algorithm.

Inspect the current integration rather than inferring it from crate names:

- [Runner](../../../utils/xtask/src/formal_test.rs) resolves workspace packages
  with Cargo metadata and reads their `hax.toml`.
- [ADT configuration](../../../libs/adt/hax.toml) contains working extraction
  scenarios and compatible tool/library pins.
- [DenseTrie proofs](../../../libs/adt/proofs/dense-trie/lean/DenseTrie/Verification/ProofObligations.lean)
  show a selected safe function and an explicit std contract.
- [IndexMap proofs](../../../libs/adt/proofs/indexmap/lean/Indexmap/Verification/ProofObligations.lean)
  show a representation invariant and library assumptions for stateful operations.
- [DenseTrie Kani harnesses](../../../libs/adt/src/dense_trie/proofs.rs) call
  private production code with full-width symbolic values.

## Set up tools and add a target

Read [setup](references/setup.md) when tools are missing, incompatible, or absent
from PATH. Preserve working installations and the repository's version pins.

Read [Hax/Lean](references/hax.md) when adding extraction scenarios, dependency
models, or Lean theorems. Read [Kani](references/kani.md) when adding symbolic
harnesses, bounds, or architecture coverage.

The common command is:

```sh
cargo xtask formal-test
```

The runner discovers all workspace packages with Lean scenarios or Kani metadata.
Use `-p tir-adt` to select one Cargo package. It regenerates each extraction,
builds its Lean project, then runs `cargo kani -p <package>` when that package
declares `metadata.kani`. Adding scenarios or harnesses needs no runner branch.

For a new package using Hax, add its own `hax.toml` and maintained Lean inputs.
For Kani, add the metadata and harnesses. Kani-only packages need no Hax
configuration or Lean installation. The runner checks tools needed by the
selected packages and skips workspace members without configured proofs.

Use xshell for any necessary xtask command changes. Xtask test targets are
deliberately disabled; validate the real command without adding xtask tests.

## Validate the claimed property

1. Run the focused extraction/theorem or Kani harness first. Confirm the intended
   theorem is imported by the Lean root and the intended harness actually ran.
2. Inspect dependency assumptions, symbolic input restrictions, size bounds, and
   architecture coverage. For stateful APIs, separate individual operation
   proofs from proofs that constructors and callers establish their invariants.
3. Check a meaningful temporary error in the actual production target, such as
   a wrong returned endpoint or stored index. The proof must fail on the claimed
   property. Preserve the current source, including existing edits; restore it
   exactly and rerun green. Serialize this with all validation of that source.
4. Run the common command for packages it supports. Keep source and proof inputs
   unchanged while their verification runs. Use the project's normal code gates
   when production or orchestration code changes; skill-only edits need skill
   validation, not compiler builds or benchmarks.
5. Inspect Git's actual file selection. Track Rust harnesses, configuration,
   handwritten Lean models/theorems, and root imports. Generated extraction,
   project files, templates, dependency caches, and solver artifacts stay ignored.
   Regenerate extraction on every formal-test run; valid dependency/build caches
   may be reused.

Treat an unsupported reachable construct, unwinding failure, timeout, skipped
target, or missing tool as an unproved case. Diagnose it without suppressing the
check or assuming the target correct. Report the proved properties, assumptions,
bounds, excluded backends, commands run, and remaining caller/lifecycle gaps.
