# Add a Hax/Lean target

## Extract the production function

Read the selected implementation and its callers. Prefer a small useful function
or cohesive group of operations whose actual code the backend supports. For a
new package, create `hax.toml` beside its Cargo manifest. Reuse compatible pins
from [ADT configuration](../../../../libs/adt/hax.toml).

A function scenario has this shape; substitute the real scenario and Rust item:

```toml
[scenario.lookup]
backend = "lean"
include = ["my_crate::module::lookup"]
```

The Rust crate/item name can differ from the Cargo package name. Use native Hax
patterns for `include`, `exclude`, and `opaque`; check the installed CLI and
[scenario documentation](https://hax.cryspen.com/manual/tools/#proof-scenarios).
Extract the targets whose correctness is claimed. An opaque dependency may have
an explicit model, but the target itself must retain its extracted body.

From that package's directory, validate and extract:

```sh
cargo hax extract --dry-run -p my-package lookup
cargo hax extract -p my-package lookup
```

Inspect extraction diagnostics and the generated function before investing in
proof authoring. Confirm branches, predicates, mutations, casts, and arithmetic
still correspond to the production code. Extracted Lean typechecking is the
first gate; extraction success alone is not a correctness proof.

## Maintain the proof inputs

By default the scenario creates `proofs/lookup/lean/`. Inspect its actual root
module name and layout instead of guessing capitalization. Maintain:

- `<Root>.lean`, importing extraction and every verification module to check;
- `<Root>/Assumptions/`, for omitted dependency types, operations, and contracts;
- `<Root>/Verification/`, for invariants and proofs about extracted functions.

Hax may initially seed these maintained files. Compare new external templates
after re-extraction, and edit the maintained files rather than the templates.
Start from [DenseTrie](../../../../libs/adt/proofs/dense-trie/lean/DenseTrie.lean)
or [IndexMap](../../../../libs/adt/proofs/indexmap/lean/Indexmap.lean).

State invariants and success postconditions over the extracted data. Prove the
actual function returns `RustM.ok` with the required result under its stated
preconditions, including valid slices and non-overflowing offsets. Derive any
auxiliary witnesses from caller-facing preconditions rather than requiring
callers to assume the desired result.

Model omitted dependencies narrowly. For example, std `partition_point` needs a
partitioned predicate and a suitable closure-effect contract. A hash table model
needs the relevant key equality/hash laws. Put these assumptions in named
contracts and prove their applicability at each call. The target's own behavior
is a theorem, not a library assumption.

Audit the axioms of each public correctness theorem. Follow the existing
`#guard_msgs in #print axioms` pattern with the fully qualified expected axiom
list. This checks foundational Lean axioms and the chosen dependency contracts;
`sorryAx` and newly introduced assumptions must not silently enter the proof.

## Keep generated files out of Git

Adapt the existing proof-directory `.gitignore` to the actual root name:

```gitignore
/llbc/
/.lake/
/aeneas-error.log
/lakefile.toml
/lean-toolchain
/lake-manifest.json
/<Root>/Extraction/
/<Root>/Extraction.lean
**/*_Template.lean
```

`<Root>` here means the generated module name, not a literal directory. Track
the ignore file and maintained proof inputs. Do not force-add generated files or
maintain patches to generated Lean. Keep the scenario's output within the
workspace. The duplicate scanner already excludes `.lake` dependency caches.

## Check the target

Run `lake build` from the generated Lean project for focused feedback. Then run
`cargo xtask formal-test -p my-package` from the workspace. The common command
uses native Hax extraction each time and regenerates project files when
`project-files` is enabled. Preserve that behavior; valid `.lake` caches can stay.
It also checks package-enabled Kani harnesses.

Complete the production mutation check and restored green run from the main
skill. Report caller invariants, library assumptions, and translation/model
trust separately from the selected function's proved postcondition. An operation
proof does not establish the entire data structure's construction or lifecycle.
