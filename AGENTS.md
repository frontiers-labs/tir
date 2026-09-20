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
2. Add minimal required testing. Do not add tests unless they are load-bearing.
   Prefer LIT-style checks over hand-rolled IR builder for snapshot tests.
   See `docs/dev_guide.md` for more information on testing.
3. A good patch is the one that removes more code than it adds.
4. When changing core structures or algorithms, always update docs with
   relevant info. Do not write new docs unless explicitly asked to.
5. Do not propose or introduce bespoke graph traversal passes. Every change
   must be driven through existing mechanisms: isel axioms, PDL rules,
   affine transformations, etc. Only deviate from these paths if explicitly
   approved by the user.
6. Rely on design documentation in `docs/design` as a summary of how TIR
   works in general.

## Minimal quality gates

- `cargo clippy --workspace --all-targets --no-deps -- -D warnings` passes cleanly
- `cargo build` compiles
- `cargo nextest r` (or `cargo test` if nextest is unavailable) passes cleanly
- `cargo fmt` has no additional format changes
- `cargo xtask fcc-torture` finds no new failures (mostly for core IR changes or FCC)
- `cargo xtask extbench run` is no worse than before change (unless explicitly approved regressions)

## Commit and PR rules

- Use conventional commits for both commits and PR descriptions
- Make commit and PR bodies concise. Only describe **why** the change is needed.
  If a small example of before/after is possible, include it.
- PR description is usually commit title and body.
- Keep it tidy: no headings, no "validation" section, no bullet lists. Simple formatting with plain text only.

## Where to put things

- `core` - base IR and dialects, shared optimizations and frameworks
- `backends/<backend name>` - real binary targets (x86, RISC-V, ARM, etc)
- `gpu` - virtual ISAs (PTX, SPIR-V, etc) and common GPU operations and optimizations; real binary targets still go under `backends/...`
- `utils` - things that are not necessarily related to compiler itself (generic algorithms and data structures, SMT utils)
- `tmdl` - dedicated TMDL DSL compiler
