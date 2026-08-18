# isasim checks

Run via the workspace LIT driver: `cargo test -p tir-lit --test lit`.

Each test assembles a `.S` program for a target (`--march`), simulates it,
and verifies the instruction trace (parser coverage) or the final register
state (execution correctness) with `filecheck`. Tests are organised by target
(`riscv`, `arm64`, `x86_64`) and kind (`parse`, `exec`).
