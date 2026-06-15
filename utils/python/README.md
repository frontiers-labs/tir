# TIR Python bindings

Pythonic access to TIR over the generic C ABI (`tir-capi`). The verbs
(parse, print, run pipelines, inspect, mutate) are hand-written; the typed
per-op constructors (`tir.builtin.addi(...)` and friends) are built at import
time from the operation schema, so they track new ops and dialects
automatically — nothing is generated to disk or committed.

## Build

```sh
cargo build -p tir-capi        # builds libtir_capi (also regenerates include/tir.h)
```

The package locates the library via `TIR_CAPI_LIBRARY`, or by searching
`target/{debug,release}`.

## Use

```python
import tir

with tir.Context() as ctx:
    module = ctx.parse_module(open("prog.tir").read())
    ctx.run_pipeline(module, "builtin.func(mem2reg)")
    for op in module.walk():
        print(op.dialect, op.name)

    # Typed constructors, derived from the schema:
    i32 = ctx.parse_type("!i32")
    block = ctx.create_block([i32, i32])
    a, b = block.args
    block.append(tir.builtin.addi(ctx, a, b, result_type="!i32"))
    print(module.to_string())

    # Backend target dialects (RISC-V/ARM64) on demand:
    ctx.register_target("riscv64")
    print(tir.supported_targets())
```

## Test

```sh
cargo run -p xtask -- python-smoke
```
