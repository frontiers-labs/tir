# Developer Guide

## Setting up Rust

The easiest way to set up Rust toolchain is with [https://rustup.rs/](https://rustup.rs/).
By default, only stable toolchain is installed. Active Rust development
also requires nightly toolchain:

```sh
rustup install nightly
```

On Linux, builds link with [mold](https://github.com/rui314/mold), so install
it from your distribution's packages.

## Building and testing

Build is done with `cargo` tool, just like any other Rust project.

```sh
cargo build
# or
cargo build --release
```

Run workspace tests through xtask. It builds the tools required by the suite
manifests and the test executables, then runs `cargo nextest`, or `cargo test`
when nextest is not installed:

```sh
cargo xtask test
```

`nextest` is much faster than the default test runner. To install it, do
`cargo install cargo-nextest`. Arguments after `--` go to the test runner
unchanged. Each local invocation builds changed tools before running checks. To
include the pinned GCC torture corpus, use `cargo xtask test --profile ci --torture`.

### Working on Nisyros

`nisyros` is the top-level C ABI offload runtime. Its header is
`nisyros/include/nisyros.h`. The Vulkan backend loads `libvulkan.so.1` at run
time and requires `VK_KHR_pipeline_binary` and
`VK_KHR_device_address_commands`. Pipeline packages are created offline;
runtime loading does not accept SPIR-V as a fallback.
`nisyros_allocation_query` reports the realized memory type, heap, and
properties of a live base address after allocation.
`nisyros_cluster_memory_heaps` enumerates heaps for local and remote devices.
`nisyros_alloc_heap_extension` can require or prefer one heap; `UINT32_MAX`
means no selection for either field.
`nisyros_backend_get_api` negotiates a sized extension table. Its
`nisyros.vulkan` table returns borrowed Vulkan instance, physical-device,
device, and queue handles. Finish relevant Nisyros work before using those
handles, finish external work before returning its memory to Nisyros, and do
not submit externally to a Nisyros-owned queue concurrently. These tables do
not load replaceable backend plugins.

Run the package parser and Vulkan FFI layout checks with `cargo test -p nisyros`.
The physical GPU check compiles its shader, captures a native
pipeline package, and tests the C ABI in a second process:

```sh
nisyros/tests/run-hardware.sh
nisyros/tests/run-peer-hardware.sh
python3 nisyros/tests/run-communication.py /usr/share/vulkan/icd.d/radeon_icd.json
python3 nisyros/tests/run-communication.py /usr/share/vulkan/icd.d/radeon_icd.json --replay
python3 nisyros/tests/run-communication.py /usr/share/vulkan/icd.d/radeon_icd.json --peer-loss
python3 nisyros/tests/run-communication.py /usr/share/vulkan/icd.d/radeon_icd.json --mismatch
python3 nisyros/tests/run-communication.py /usr/share/vulkan/icd.d/radeon_icd.json --collective-mismatch
python3 nisyros/tests/run-communication.py /usr/share/vulkan/icd.d/radeon_icd.json --collective-timeout
```

The first script requires `glslangValidator`, `spirv-val`, the Vulkan validation
layer, and a device with the strict Vulkan profile. CI runs C and Python ABI
checks against a software Vulkan ICD. A software device lacking the mandatory
extensions must be reported as unsupported. CI also starts two processes to
check fixed-membership discovery, stable cluster IDs, and ordered group ranks.
The hardware script probes cooperative-matrix tuples and, where the FP16 input
and FP32 accumulation tuple is available, captures and executes a separate
version-two package. Package loading checks exact tuple membership and enabled
feature bits before importing the binary. Bfloat16 and shader enqueue remain
unavailable when their device capabilities are absent.

The peer script duplicates one ICD manifest and verifies FD-backed memory and
timeline sharing between two logical views of the same GPU. It does not test
two distinct physical GPUs. Set `NISYROS_VULKAN_ICD` to select another ICD.
The communication script runs two processes, with two local ranks on node zero.
It checks matched transfers, device visibility after receive completion,
sequence-bound replay, and error propagation for peer loss and byte-count
disagreement. It also checks three-rank unsigned and signed 32-bit reductions,
five sequence-bound collective replays, metadata mismatch, and unmatched
collective timeout.
Pass `simulator` in place of the ICD path to run the same communication cases
against `nisyros/tests/vulkan_simulator.c`. CI also packages the checked-in
`add_one.spv` fixture and runs the C kernel test through strict binary loading.
The simulator recognizes only that fixture and models its result on coherent
host storage. It does not test driver conformance, GPU cache visibility, or
actual shader execution.
`local_communication.c` runs a two-rank group in one process with no network
worker. It checks an in-place reduction. The same local matching path also
runs inside the two-node test.

For local performance data, run `nisyros/tests/run-performance.sh <output.csv>`
with the intended Vulkan ICD selected. It records individual empty and
two-segment submit latencies, submit-thread CPU time, and validated 64 MiB
transfer times. For host-staged TCP progress, run
`python3 nisyros/tests/run-communication.py <icd.json> --performance <output-dir>`.
That run records per-message wall and process CPU time from both processes.
Use `python3 nisyros/tests/summarize-performance.py <output.csv> <output-dir>`
to print medians and 95th percentiles from the retained raw samples.

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

[suite.tools]
tir = "tir"
```

The globs are relative to the suite directory; a leading `!` excludes. Each
selected check file shows up as its own test case named by its path relative to
the workspace root (e.g. `core/checks/Restructure/while-loop.tir`). It runs
through xtask:

```sh
cargo xtask test -- -E 'binary(lit)'
```

Like LLVM LIT, the driver filters by environment variable: `LIT_FILTER` is a
regex selecting the tests to run, `LIT_FILTER_OUT` excludes matching tests.

```sh
LIT_FILTER='^fcc/checks/Codegen' cargo xtask test -- -E 'binary(lit)'
```

`[suite.tools]` maps commands in `RUN:` lines to Cargo binary names. Xtask
discovers these requirements and supplies the freshly built tool directory.
The LIT harness never builds tools itself. Running it directly without its
required binaries reports the affected suites as ignored.

CI packages a build once, then runs the tests in separate shards:

```sh
cargo xtask test-pack --profile ci --torture --output target/ci-tests.tar.zst
xtask test --archive target/ci-tests.tar.zst -- --partition hash:1/8
```

The archive contains the test executables, CLI tools and torture corpus.
Archive execution extracts them without rebuilding and reads fixtures from the
checkout it runs in, which must be the packed commit. Packing and archive
execution require nextest. All shards must use the
same archive; their union must cover the unsharded test inventory exactly once.

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
unsuccessful solves. Dump failures produce a
warning on stderr and do not stop compilation. Files use the name
`<kind>-<pid>-<sequence>.json`. The compiler never replaces an existing file.

Each file contains one version 1 `tir-pbqp` JSON object. `kind` is `regalloc`.
`node_costs` holds one cost vector per node. Each `edges` entry has
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

Benchmarks live in each package's `benches` directory and are declared in its
`Cargo.toml` with `harness = false`. Run one target with
`cargo xtask bench -- -p <package> --bench <target>`.

Function benchmarks use native Criterion. See `core/benches/lexer.rs` for a
complete example.

Use `b.iter_batched` when each iteration needs fresh input prepared outside
measurement. `tir_bench::criterion_group!` takes 10 samples after Criterion's
3 seconds of warmup, over 5 seconds of measurement. Individual benchmarks can
override these settings.
Pass `-- --help` to a Criterion target for its filtering and timing options.

External commands use `tir_bench::Experiment` from `utils/bench`:

```rust
use tir_bench::Experiment;

let log = Experiment::new("compile/example", "gcc")
    .args(["-c", "example.c", "-o", "example.o"])
    .run()?;
```

An experiment repeats until it has both 3 successful runs and 1 second of
measured time. Override these minimums with `min_repetitions` and
`min_duration`. Timing includes process startup and waiting for exit.
`run()` returns the combined stdout and stderr. Use `run_validated(validate)`
to check that output before saving measurements. The program benchmarks in
`fcc/benches` name each compiler and level, so
`cargo xtask bench -- --bench coremark fcc/coremark/-O2` runs one of them.

Run `cargo xtask bench` to run the workspace benchmarks with
`cargo nextest bench` and print a summary. nextest's default profile leaves
out the gcc, clang, egg and egglog baselines, which do not change with TIR;
`NEXTEST_PROFILE=perf cargo xtask bench` runs them too, as CI does. Without
cargo-nextest the xtask runs `cargo bench`, baselines included. Pass Cargo
target selection and filters after `--`, for example
`cargo xtask bench -- -p fcc --bench coremark`. Use `--profile <profile>`
before `--` to change the compiler and benchmark profile.

Results default to `target/criterion`, using the configured Cargo target
directory when it differs from `target`. `CRITERION_HOME` overrides the results
directory. Criterion saves `new/sample.json` and `new/benchmark.json` under each
benchmark directory. Experiments save `new/bencher.json` with the mean latency
and each process duration in nanoseconds; reserved characters in names become
`_` in directory names.

The xtask report includes measurements written during that invocation and saves
`benchmarks.csv` in the Cargo target directory. It reports the sample count,
minimum, maximum, median, mean, sample standard deviation, and iterations per
second. Criterion durations are normalized per iteration. Experiment durations
measure one command execution.

### Running fuzz tests

We also have fuzzing set up for user-facing parsers. These tests require
`cargo-fuzz`, which can be installed with `cargo install cargo-fuzz`.

```sh
# List fuzz targets.
cargo fuzz list

# Make sure all fuzz binaries still compile.
cargo check -p tir-fuzz --bins

# Run one target for a bounded local smoke campaign.
cargo fuzz run tmdl-fuzz -- -max_total_time=60
cargo fuzz run riscv-assembly-fuzz -- -max_total_time=60
cargo fuzz run arm64-assembly-fuzz -- -max_total_time=60
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
4. Light utility crates with small dependency footprints (`libs/adt`,
   `utils/arrange`, `utils/filecheck`, `utils/lit`, `utils/xtask`) keep
   their unit tests in-crate.
