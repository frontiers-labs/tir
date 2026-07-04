# tir-jit

A minimal in-process JIT for TIR modules on Linux. Give it a TIR IR module as
text; it runs the shared backend pipeline (instruction selection, register
allocation, finalization), maps the machine code into executable memory,
resolves relocations against runtime addresses, and returns callable function
pointers.

Intended for benchmarking harnesses: generate a microkernel, compile it, and
call it from the host — optionally with a performance counter enabled around the
call (see `examples/benchmark.rs`).

## Usage

```rust
use tir_jit::Jit;

let jit = Jit::host()?;                       // target the host architecture
let module = jit.compile(r#"
    module {
      func @add(%0: !i64, %1: !i64) -> !i64 {
        %2 = addi %0, %1 : !i64
        return %2
      }
      module_end
    }
"#)?;
let add: extern "C" fn(i64, i64) -> i64 = unsafe { module.get("add") }.unwrap();
assert_eq!(add(2, 40), 42);
```

Host functions are made callable from the module by name:

```rust
extern "C" fn host_triple(x: i64) -> i64 { x * 3 }

let mut jit = Jit::host()?;
jit.define_symbol("host_triple", host_triple as *const std::ffi::c_void);
// a `call @host_triple(...)` in the IR now dispatches to the host function.
```

## Scope

- **Linux only.** Uses `mmap`/`mprotect` for executable memory.
- **Targets:** x86-64 and AArch64 are validated; RISC-V relocation support is
  best-effort (PC-relative branches only, no external-call trampolines).
- **Calling convention:** compiled functions follow the target C ABI (System V /
  AAPCS), so they are callable as `extern "C"` pointers. Only register-passed
  arguments are supported, matching the backend's ABI lowering.
- **Single read+execute mapping:** code and read-only data share one region;
  writable globals are not supported.
- **External calls** into the host are routed through per-symbol trampolines so a
  64-bit host address is reachable from a short pc-relative branch.

Current backend limitations propagate here: a value that stays live across a
call (which would need a callee-saved register) and block arguments on branch
edges are not yet handled by register allocation.
