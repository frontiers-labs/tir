# The TIR project guidelines

TIR (Target Intermediate Representation) is a scalable compiler framework
inspired by MLIR. It still provides a notion of dialects and interfaces
for describing custom behavior and generic transformations. But unlike
traditional compilers, TIR prefers use of formal methods and math-backed
algorithms over hand-rolled graph traversal transformations. These rewrites
ride on strict e-graph or polytope model drivers. We provide two custom
DSLs for these purposes: TMDL allows one to describe target ISAs in great
detail, serving as a unified Architecture Description Language for both
instruction behavior and uarch performance details; PDL describes mid-end
instcombine-style transitions between concrete operations. TIR also expands
beyond traditional SSA form and allows representing programs as RVSGD.

Long-term goal for TIR is to be a generic framework for building classic
and special purpose compilers (AI, HDL, etc) while taking advantages of
modern day architectures (SIMD, parallelism) both inside the compiler
and in the compiled code. At this point compiler is very new and lags
behind in both compile time and runtime performance compared to GCC and
Clang. Over the course of next months we hope to close these gaps entirely:
provide a C compiler that emits code just as good (and better!) and takes
the same amount of time (or less!) to do so.

## Coding guidelines

1. Produce minimal change required to achieve the goal. If it's a bug fix,
   changes must address bug root cause only. If it's a new feature, commit
   must contain only the minimal amount of code to make tests pass.
2. A good patch is the one that removes more code than it adds.
3. Do not touch docs unless the task asks for it. Docs must not restate
   what the code says.
4. Do not propose or introduce bespoke graph traversal passes. Every change
   must be driven through existing mechanisms: isel axioms, PDL rules,
   affine transformations, etc. Only deviate from these paths if explicitly
   approved by the user.
5. Rely on design documentation in `docs/design` as a summary of how TIR
   works in general.
6. See `Source overview` section of README.md before writing new code.
7. For working on new passes, start with a prototype via `tir egglog` command.

## Writing new tests

Add minimal amount of required testing to prove your changes work. Do not add new tests unless they are load-bearing.

Key requirements summary:

1. Pick only one appropriate tool in this order:
   - Formal verification - prefer when the module can be formally described and verified in reasonable time. The instruments include Hax (with Lean backend), Kani, Isla golden references. Use `formal-verification` skill for more info.
   - Property exhaustion testing - proptest has a few precedents in the repository.
   - Snapshot testing - LLVM LIT-style tests for capturign the effect of a pass or transformation
   - Unit testing - only use as the last resort when other methods are unsuitable
2. Avoid tests on the hot build path - many crates use tests outside of crate tree to avoid rebuilding library twice for `cfg(tests)`. This is intentional pattern introduced to optimize CI and local build times.
3. Only test public surfaces. Do not freeze implementation details in tests.
4. Never construct IR manually in unit tests.

See `docs/dev_guide.md` for more information on testing.

## Minimal quality gates

- `cargo xtask lint` passes cleanly (clippy, rustdoc, ast-grep rules in
  `.config/ast-grep/`, duplicate code; needs `uv`)
- `cargo build` compiles
- `cargo xtask test` passes cleanly
- `cargo fmt` has no additional format changes
- `cargo xtask test --profile ci --torture` finds no new failures
  when changing core IR or FCC
- `cargo xtask bench` is no worse than before change (unless explicitly approved regressions).
  Run benchmarks through it, not `cargo bench`, which skips the filters.
- `cargo xtask formal-test` verifies correctly. Only run when you change code that is directly
  covered by formal verification tools like Hax or Kani.

Be mindful of what you run. Documentation, comments change, isolated crates do not need full tests or benchmarks.
Only run gates for paths that are actually affected.

## Commit and PR rules

- Use conventional commits for both commits and PR descriptions
- Make commit and PR bodies concise. Only describe **why** the change is needed.
  If a small example of before/after is possible, include it.
- PR description is usually commit title and body.
- Keep it tidy: no headings, no "validation" section, no bullet lists. Simple formatting with plain text only.
