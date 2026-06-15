# TIR Python bindings

Pythonic access to TIR over the generic C ABI (`tir-capi`). The verbs
(parse, print, run pipelines, inspect, mutate) are hand-written; the typed
per-op constructors in `tir/_ops.py` are generated from the operation schema by
`tir-bindgen`, so they track new ops automatically — nothing per-op is written
by hand.

## Build

```sh
cargo build -p tir-capi                                   # builds libtir_capi
cargo run -p tir-bindgen -- --lang python \
    --output utils/python/tir/_ops.py                     # regenerate _ops.py
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

    # Typed, generated constructors build ops by name:
    i32 = ctx.parse_type("!i32")
    block = ctx.create_block([i32, i32])
    a, b = block.args
    block.append(tir.builtin.addi(ctx, a, b, "!i32"))
    print(module.to_string())
```

## Test

```sh
cargo run -p xtask -- python-smoke
```
