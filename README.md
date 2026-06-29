[![Coverage Status](https://coveralls.io/repos/github/frontiers-labs/tir/badge.svg?branch=master)](https://coveralls.io/github/frontiers-labs/tir?branch=master)

# Target Intermediate Representation

TIR project aims to provide a flexible compiler and binary analysis toolchain,
including various transformations, optimizations and executable generation.
It is inspired by LLVM, Bolt and MLIR. At the heart of TIR is a generic
intermediate representation and a number of purpose-specific dialects.
Users can use it to create new tools by introducing new dialects,
optimizations or use it TIR as a generic assembler tool.

## Short-term goals

TIR is a research project. Unlike MLIR, which is very similar in the spirit,
TIR focuses on classic compilers and transformations exclusively (at least
for now). The success criteria for the research is to:
1) Have a simple C compiler capable of compiling a simple project, like SQLite
2) Have a functional RISC-V and/or AArch64 backend
3) Have competitive performance on SQLite benchmarks

Once completed, the project is supposed to be good enough to teach students
basics of compiler construction and do research in the field of compiler
optimizations, including but not limited to ML-driven, affine, solver-based
techniques or creating ISA extensions. 

## Project overview

- `backends/`
  - [`common/`](./backends/common/) - common utils for binary code generation
  - [`riscv/`](./backends/riscv/) - generic RISC-V backend
  - [`x86_64/`](./backends/x86_64/) - prototype x86-64 backend
- [`core/`](./core/src) - generic IR definitions
  - [`src/builtin/`](./core/src/builtin/) - builtin dialect, roughly implementing
    functionality of LLVM IR
- [`tools/`](./tools/) - tools meant to be distributed as part of the toolchain
- [`utils/`](./utils/) - internal utilities, primarily for testing purposes

## Building from source

TIR is a Rust project, and can be built with `cargo`, just like any other Rust
project. If you want to contribute to this repository, refer to our
[Developer guide](docs/dev_guide.md) and [Contribution guide](./CONTRIBUTING).
