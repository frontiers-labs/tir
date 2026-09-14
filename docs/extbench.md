# External benchmarks

`cargo xtask extbench compile` compiles each selected source with every compiler
in its suite. It reports wall time, peak RSS in KiB, and configured diagnostic
metrics. `cargo xtask extbench run` compiles and links outside the measurement,
then times each executable on the current host. A failed compile, link, or run
fails the command and prints the process output.

The FCC suite compares FCC, GCC, and Clang at `-O0` and `-O2`. Install GCC, Clang,
and `/usr/bin/time` before running the whole suite. FCC builds in release mode
once before measurement. Linux and macOS provide the supported RSS collectors.

```sh
cargo xtask extbench compile --package fcc --bench coremark
cargo xtask extbench run -p fcc -b coremark
cargo xtask extbench compile -p fcc -b 'core*' --compiler clang
cargo xtask extbench compile -p fcc -b coremark --input llvm
cargo xtask extbench run -p fcc -b coremark --input llvm
cargo xtask extbench compile --list
cargo xtask extbench compile -p fcc --no-build --output samples.json
cargo xtask extbench compile -p fcc --baseline samples.json --output current.json
```

`--package` selects an exact package name. `--bench` selects benchmark directory
names with a glob. `--compiler` selects one configured compiler. `--list` lists
benchmarks without fetching or building. `--suite path/to/bench_suite.toml`
selects an explicit suite. By default the command discovers suite manifests in
the workspace, excluding hidden, generated, and LIT `Inputs` directories.
`--input llvm` asks Clang to prepare LLVM IR and benchmarks compilers configured
to consume it. The default, `--input source`, keeps the existing source path.
The FCC suite enables LLVM input for CoreMark and Dhrystone.

## Benchmark directories

The FCC suite lives in `fcc/extbench/bench_suite.toml`. Each direct child
containing `benchmark.toml` is a benchmark. Adding a benchmark requires a new
directory and its contents. No Rust registration is needed.

A local benchmark manifest can contain:

```toml
sources = ["*.c", "!unused.c"]
inputs = ["source", "llvm"]
flags = ["-I."]
link_flags = ["-lm"]
args = ["1000"]
levels = ["-O0", "-O2"]
```

Sources are globs relative to the benchmark directory. A leading `!` excludes a
source. Flags, link flags, and arguments default to empty lists. Levels default
to `-O0` and `-O2`; a suite for another language can use its own level strings.
Sources are sorted before compilation. Each argument remains one argument,
including arguments containing spaces.

`inputs` lists the accepted input paths. It defaults to `["source"]`, so existing
benchmarks retain their behavior. The runner omits benchmarks that do not list
the requested input. CoreMark and Dhrystone list both paths.

`separate = true` links and executes every source individually. GCC torture uses
this setting. By default all source objects link into one executable.

`exclude_files` lists text files relative to the benchmark directory. Each line
names an excluded source relative to the source directory. Blank lines and
`#` comments are ignored. Torture reuses the existing known-failure files.

A pinned upstream checkout replaces the local source directory when specified:

```toml
[source]
repository = "https://github.com/eembc/coremark.git"
revision = "1f483d5b8316753a742cbf5590caf5bd0a4e4777"
```

`revision` must be a full commit hash. Optional `subdir` selects a directory
inside the checkout and enables sparse checkout. Downloads are cached under
`target/extbench/sources`. Build products use a temporary directory per
invocation and are removed on completion. Benchmark programs run from their
source directory, so local input files are available.

## Compiler configuration

A suite defines a package name and compiler command arrays:

```toml
[suite]
package = "another-package"

[[compiler]]
name = "compiler-name"
input = "llvm"
build = ["cargo", "build", "--release", "-p", "another-package"]
compile = ["compiler", "{level}", "{flags}", "-c", "{source}", "-o", "{output}"]
link = ["compiler", "{objects}", "{link_flags}", "-o", "{output}"]
```

`build` is optional and runs from the workspace root. Compile and link commands
run from the source directory. The runner imposes no source extension or
language flags. Compiler commands and benchmarks define those choices. `input`
defaults to `source`; set it to `llvm` for a compiler that consumes the suite's
prepared LLVM IR.

| Placeholder | Value |
| --- | --- |
| `{root}` | Absolute workspace directory |
| `{arch}` | Host architecture in TIR target spelling |
| `{source}` | Absolute source path for compilation |
| `{output}` | Absolute output path for compilation or linking |
| `{level}` | Current benchmark level |
| `{flags}` | Benchmark compile arguments |
| `{objects}` | Compiled object paths for linking |
| `{link_flags}` | Benchmark link arguments |

List placeholders must occupy an entire array element. Single-value
placeholders also work inside an element, such as
`{root}/target/release/fcc`. Commands execute as argument arrays. Shell syntax
requires an explicit shell command in the configuration.

`[compiler.env]` sets environment variables for compiler commands.
`[compiler.metrics]` maps metric names to regular expressions. The first capture
group must contain a finite nonnegative number. The runner sums repeated matches
from compiler stderr. Missing configured metrics fail the measurement.

Suites that support LLVM input also configure the producer:

```toml
[llvm]
prepare = ["clang", "{level}", "{flags}", "-S", "-emit-llvm", "{source}", "-o", "{output}"]
version = ["clang", "--version"]
```

The runner expands the same source flags and optimization level used by the
source path. It runs IR preparation before the measured compiler command. Run
mode also keeps benchmark compilation and linking outside the execution timer.
TIR reports `import_ms` for input reading, LLVM import, target setup, and
verification, and `backend_ms` for lowering and object emission. Clang's IR
preparation time is outside both intervals.

FCC enables `TIR_TIME_PASSES` and reports three disjoint wall-clock intervals:

- `frontend_ms` covers target setup, preprocessing, parsing, semantic analysis,
  IR generation, and mandatory frontend lowering through restructuring.
- `passes_ms` covers the selected middle-end pipeline and data lowering.
- `backend_ms` covers instruction selection, allocation, finalization, and
  object emission.

These intervals exclude source-file reading, process startup, and linking.
The existing per-pass report includes backend passes and can sum worker times;
it is not used as the disjoint `passes_ms` interval. GCC and Clang report total
wall time and RSS. Their absent phase metrics are omitted, not estimated.

### Rule application counts

With `TIR_RULE_STATS=1`, saturation writes one `tir-rule:` line per rule to
compiler stderr:

```text
tir-rule: pass=instcombine-nodes rule=sub-zero applications=3 changed=1 noop=2
```

`applications` counts executed rule matches. `changed` counts applications
that added nodes, merged classes, or raised facts; `noop` counts the rest.
Counts include post-saturation rules and work in temporary assumption scopes.
They measure saturation activity, not rewrites retained by final extraction.
Rules with no applications are included. Counts aggregate by rule name within
each pass report and reset after reporting. Sum repeated lines across functions
and translation units for a benchmark total.

Set `TIR_TIME_PASSES=1` separately when you also need pass timing and round
telemetry. For example, `TIR_RULE_STATS=1 target/release/fcc -O2 -c input.c -o
input.o 2>rules.log` captures rule counters without timing diagnostics. The
extbench runner does not retain raw compiler stderr in its result JSON.

## Results and baselines

JSON records the mode, input kind, host architecture and OS, and samples keyed
by package, benchmark, compiler, level, and source. LLVM results also record the
producer version, command settings, and a SHA-256 digest of the prepared IR.
Each sample contains `wall_ms`,
`peak_rss_kb`, and a map of configured metrics. RSS is the maximum reported by
`time`, including waited-for child processes. It is not the sum of simultaneous
process memory. Run samples for a linked benchmark use `source = "run"`.

Baseline comparisons require the same input kind, then use shared sample keys.
Totals are grouped by package, compiler, and level. GCC and Clang are still
reported, but only FCC and TIR fail the command when total wall time grows by
more than 10%, summed per-source peak RSS
grows by more than 2%, or one peak grows by more than 35%. Host compilers move
with the runner; FCC and TIR are the compilers this tree builds. The command rejects a
different mode or a baseline with no matching samples. New samples remain in the
output but do not contribute to the baseline comparison. Use the same host,
compiler versions, flags, and inputs for comparable results. Samples are written
before the baseline verdict.

The nightly job uses the new JSON format and starts a new baseline history.
`cargo xtask gate` retains its original FCC/GCC baseline format and thresholds;
it reads its pinned benchmark sources and flags from the new manifests.
