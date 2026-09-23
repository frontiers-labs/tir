# Developer Guide

## Setting up Rust

The easiest way to set up Rust toolchain is with [https://rustup.rs/](https://rustup.rs/).
By default, only stable toolchain is installed. Active Rust development
also requires nightly toolchain:

```sh
rustup install nightly
```

## Building and testing

Build is done with `cargo` tool, just like any other Rust project.

```sh
cargo build
# or
cargo build --release
```

Tests can also be done with `cargo test` command, but a much better way is to
use `nextest` tool. To install it, do `cargo install cargo-nextest`. Then run
tests with the following command:

```sh
cargo nextest r
```

`nextest` is much faster than the default test runner.

### Working on the C frontend

FCC keeps its public entry points in `fcc/src/parser.rs`, `sema.rs`,
`codegen.rs`, and `diagnostics.rs`. Their child modules group the implementation
by responsibility:

- `parser/` contains declarations, expressions, statements, and syntax-level
  language checks. `lang_options.rs` holds the selected standard and dialect.
- `sema/` contains C types, target properties, declarations, expressions,
  statements, conversions, initialization, and diagnostic standard references.
  Semantic analysis applies semantic language-version checks, annotates the
  existing AST, and returns `TypedAst`.
- `codegen/` contains ABI classification, calls, scalar operations, expressions,
  control flow, initialization, and data lowering. It consumes `TypedAst` and
  emits the existing TIR and CIR operations.
- `diagnostics/` contains source storage, the typed diagnostic catalog, and
  diagnostic construction and rendering. The catalog supplies stable codes and
  the text used by `fcc --explain`.

### Running check tests

Check-tests are very similar to LLVM Integrated Tests (LIT). Each test file
lives under a crate's `checks/` directory and contains one or more `RUN:`
lines that pipe a tool's output into `filecheck`, e.g.:

```
// RUN: tmdlc --action=emit-ast --output=- %S/../Inputs/simple.tmdl | filecheck %s
// CHECK: File {
// CHECK-NEXT:     items: [
```

A single integration test in `utils/lit` discovers every test suite in the
workspace. A suite is a directory holding a `test_suite.toml` that names it and
globs its test files:

```toml
[suite]
name = "TIR core IR and pass checks"
glob = ["**/*.tir", "!**/Inputs/**/*"]
```

The globs are relative to the suite directory; a leading `!` excludes. Each
selected check file shows up as its own test case named by its path relative to
the workspace root (e.g. `core/checks/Restructure/while-loop.tir`). It runs as
part of `cargo test`, or standalone:

```sh
cargo test -p tir-lit --test lit
```

Like LLVM LIT, the driver filters by environment variable: `LIT_FILTER` is a
regex selecting the tests to run, `LIT_FILTER_OUT` excludes matching tests.

```sh
LIT_FILTER='^fcc/checks/Codegen' cargo test -p tir-lit --test lit
```

`filecheck` is a small, self-contained reimplementation of LLVM's FileCheck
(built on `chumsky` and `ariadne`); it lives in `utils/filecheck` and is also
available as a standalone binary. The LIT driver is in `utils/lit`.

The `CHECK` lines for golden-output tests are generated, not written by hand.
Regenerate them after an intentional output change with:

```sh
cargo build                              # build the tools first
./utils/scripts/update_checks.py tmdl    # or: fcc, ...
```

Pass explicit file paths to (re)generate specific tests, including brand-new
ones. Hand-authored tests (those without the generated header) are never
touched by a bulk regeneration.

### Dumping PBQP tasks

Set `TIR_PBQP_DUMP_DIR` to capture each PBQP task that reaches the solver:

```sh
TIR_PBQP_DUMP_DIR=/tmp/pbqp fcc compile --stage asm --march riscv64 input.c -o output.s
```

An unset or empty value disables dumping. The compiler creates the directory
and writes one file per solve immediately before the solve. This includes
register allocation retries and unsuccessful solves. Dump failures produce a
warning on stderr and do not stop compilation. Files use the name
`<kind>-<pid>-<sequence>.json`. The compiler never replaces an existing file.

Each file contains one version 1 `tir-pbqp` JSON object. `kind` is `isel` or
`regalloc`. `node_costs` holds one cost vector per node. Each `edges` entry has
`lhs`, `rhs`, and `matrix` IDs. Each `matrices` entry has `rows`, `cols`, and a
flat `costs` array. Node IDs and matrix IDs are zero-based array positions.
Edges are sorted by `(lhs, rhs)`, where `lhs < rhs`. Matrix rows index the `lhs`
node's alternatives, and columns index the `rhs` node's alternatives. The
`costs` array is row-major. The dump contains only matrices referenced by an
edge, and shared matrices keep one matrix ID.

`inf_cost` records the solver's infinity threshold. Costs at or above this
threshold are forbidden by the objective, but the dump preserves every input
cost as an unsigned 64-bit JSON integer. Readers must preserve integer
precision. Python's `json` module does so. Parsing through a binary64 number
can change costs.

The dump records the mathematical task before solver reductions. It does not
contain a solution.

### Running benchmarks

Run `cargo bench` from the workspace root to measure Rust functions and the
compilation/runtime of the C workloads. Cargo builds the compilers in its bench
profile. Workloads are Rust modules under `benchmarks/programs`; `utils/bench`
provides the shared harness. No benchmark TOML files are needed.

```sh
cargo bench -- --list
cargo bench -- --filter '*dhrystone*' --level O2 --samples 5
cargo bench -p fcc --bench programs -- --compiler fcc --phase compile
cargo bench -- --engine cachegrind --filter '*dhrystone*' --phase compile
cargo bench -- --cpu 2
```

The full default workload includes the pinned GCC torture corpus and can take a
long time. Filtering does not change workload arguments. The harness's `--list`
does no corpus fetching or benchmark compiler invocation; Cargo may build the
targets first. Git corpora are cached in `target/bench-sources`;
`TIR_BENCH_SOURCE_CACHE` overrides that path, and `--offline` rejects cache misses.
CoreMark and Dhrystone also run through the LLVM-input target in `tir-tools`,
sharing prepared IR with the Clang backend controls.

Each Cargo target writes a unique bundle under `target/bench`, or `--output DIR`.
Relative output and baseline paths resolve from the workspace root.
`summary.bmf.json` uses Bencher Metric Format. `results.json` retains samples,
workload identity, environment and status; command logs and Cachegrind profiles
remain alongside it. A failed run never becomes a complete baseline. Compare an
identical selection with `--baseline PATH`, naming its bundle or `results.json`.
Use `--min-cases N` in a focused CI job to reject empty selections.

Native runs report nanoseconds and peak process RSS in bytes. RSS is the kernel
process high-water mark, not simultaneous process-tree memory. External process
measurements require GNU `/usr/bin/time` as a small accounting supervisor: direct
`wait4` RSS includes the spawning harness's memory on Linux. RSS is read from a
separate output file; wall and CPU time include supervisor overhead. Function cases
exclude declared setup and report per-operation time; they do not report RSS.
Program output verification runs outside measurement. Compilation validates the
initial executable and caches successful object hashes; unchanged outputs need
only a hash check, while changed outputs are linked and validated again. Compiler variants run in
rotated order with 3 warmups and 15 samples by default; override these explicitly
with `--warmups` and `--samples`. Native function calibration discards a cold call
and grows warm batches toward `--sample-time-ms`, which defaults to 100 ms.
Compiler phase diagnostics are collected in
untimed preparation; primary timing runs do not enable that instrumentation.

Cachegrind requires Valgrind on PATH and records instruction/cache/branch counts
instead of instrumented time or RSS. It runs one fixed-work sample. Child execution tracing defaults off and compiler
helpers enable it for GCC/Clang driver processes. Rust function
profiling additionally requires `--features tir-bench/cachegrind` on the Cargo
command and the headers and executable from Valgrind 3.22 or newer. Install
the headers before building. For an isolated installation, set
`VALGRIND_REQUESTS_VALGRIND_INCLUDE` to its include directory. If the headers were
installed after a native build, rebuild the bindings with
`cargo clean -p valgrind-requests` before profiling. Use
`--iterations N` to choose a fixed operation count. Counts are useful in shared
CI but are not a substitute for native performance measurements.

Use a dedicated Linux runner for native regression gates. Pin an allowed CPU,
reserve its sibling cores, and control frequency/boost policy and background
work. The harness takes a cooperative host lock, checks that a requested CPU is
allowed, and verifies affinity before completing the run. It records the host
and tool identities. Frequency, thermal and background-load control belong to
the runner configuration; the harness does not tune or audit privileged host
policy. Pin software and source versions too. A container alone does not control
host contention.

Adding a workload means adding a Rust module and registering it in the package's
Cargo harness. Shared command/source helpers handle common preparation; custom
Rust setup and validators can define new behavior without changing a manifest
schema. Keep input identities and fixed arguments explicit so incompatible
baselines are rejected. Function benchmarks can call `set_contract_version` to
mark changes to their input generation or measurement boundaries. The optional
`cachegrind` feature builds client bindings and requires Clang/libclang. Native
benchmarks do not require that feature, those bindings, or Valgrind.

Use `cargo xtask fp-check` to record pinned GCC floating-point observations,
compare cumulative semantic requirements, and summarize saved reports.
The case manifest is `fcc/checks/Inputs/fp/cases.toml`. Each case records its ID,
owning stage, inputs, and independent expectation. GCC-specific expectations
use `reference_expectation`.

```sh
cargo xtask fp-check reference --gcc gcc --output /tmp/fp-reference.json
cargo xtask fp-check check --stage reference --reference /tmp/fp-reference.json --output /tmp/fp-check.json
cargo xtask fp-check report /tmp/fp-check.json
```

The default reference profile requires GCC 15.2. Use `--case ID` to select one
case. Reports record compiler identity, commands, source and manifest digests,
observations, and each case's status. `unsupported_capability` and
`missing_infrastructure` never count as passes. The report command fails if
any case is not `pass` or the report contains no cases.

### Running fuzz tests

We also have fuzzing set up for user-facing parsers. These tests require
`cargo-fuzz`, which can be installed with `cargo install cargo-fuzz`.

```sh
# List fuzz targets.
cargo fuzz list

# Make sure all fuzz binaries still compile.
cargo check -p tir-fuzz --bins

# Run one target for a bounded local smoke campaign.
cargo +nightly fuzz run tmdl-fuzz -- -max_total_time=60
cargo +nightly fuzz run riscv-assembly-fuzz -- -max_total_time=60
cargo +nightly fuzz run arm64-assembly-fuzz -- -max_total_time=60
```

### Nightly fuzz defects

The nightly fuzzers file one issue per defect, not one per red job. A defect is
identified by a signature over its reduced case and the passes that miscompile
it, so the same bug found from a different seed lands on the issue that already
tracks it instead of opening a new one.

Each issue carries the case that fails, the pass the divergence was narrowed to,
and a command that reproduces that one failure. It also embeds the record itself
in an HTML comment at the bottom; the next nightly replays every open defect from
that record and closes the ones that no longer reproduce. Editing or deleting the
comment stops the issue from ever closing itself, so leave it alone.

To work with a record by hand:

```sh
# Print a recorded defect as the issue it becomes.
cargo xtask fcc-fuzz --render target/fuzz/failures/<signature>.json

# Re-run every defect currently tracked on GitHub.
gh issue list --state open --label fuzz --limit 500 --json body \
  | cargo xtask fcc-fuzz --extract tracked
cargo xtask fcc-fuzz --replay tracked
```

### Collecting coverage info


**WARNING!!!** Coverage tool creates a lot of temp files in your working
directory. You better commit all your changes to be able to use git to
clean up.

Install dependencies:

```sh
rustup component add llvm-tools-preview
cargo install grcov
```

Run tests with special flags:

```sh
CARGO_INCREMENTAL=0 RUSTFLAGS='-Cinstrument-coverage' LLVM_PROFILE_FILE='cargo-test-%p-%m.profraw' cargo test
grcov . --binary-path target/debug/ -s . -t coveralls+ --branch --llvm \
    --ignore '../*' --ignore "/*" --ignore 'macros/*' --ignore 'fuzz/*' \
    --ignore '**/tests/**' -o target/coverage/html
```

Open `target/coverage/html/index.html` to see the report. Coverage is a local
tool only; CI does not collect it.

## Test policy

Every new contribution must be accompanied by reasonable amount of testing.
Tests must demonstrate the intention behind the changes and make it easier
to understand what the code is doing.

1. Prefer check tests (a.k.a. LIT-style tests) over other kinds of tests
   whenever possible. Checks are a good option for testing passes, code
   generation, any other behavior observed via CLI. They require less
   code to set up and are easier to understand.
2. Heavy crates (core, backends, simulators, fcc, tmdl, symbolic, …) build
   with `test = false`. Their unit tests live in `utils/unit-tests`, one
   `#[cfg(test)]` module per crate, forming a single test binary.
3. Only public APIs get tests. Never expose private surface (visibility
   changes, `#[doc(hidden)]`, re-exports) to make something testable; a test
   that needs private access is rewritten against public API or deleted.
4. Light utility crates with small dependency footprints (`utils/adt`,
   `utils/arrange`, `utils/graph`, `utils/filecheck`, `utils/lit`,
   `utils/pbqp`, `xtask`) keep their unit tests in-crate.
