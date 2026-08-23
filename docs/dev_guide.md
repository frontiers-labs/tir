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
   `utils/graph`, `utils/filecheck`, `utils/lit`, `utils/pbqp`, `xtask`)
   keep their unit tests in-crate.
