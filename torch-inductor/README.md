# tir-torch — a Torch Inductor backend prototype

A prototype that lets TIR act as a `torch.compile` backend: it captures the FX
graph of a model's forward pass and lowers it into TIR.

It has two parts:

- **`src/lib.rs`** — the `torch` IR dialect. One ranked tensor type
  (`!torch.tensor<f32, 1, 4, 8>`) and one op per FX node kind that the demo
  graph contains (`get_attr`, `arange`, `embedding`, `add`, `matmul`,
  `mul_scalar`, `linear`, `layer_norm`, `gelu`, `softmax`, `transpose`, `view`,
  `split`, `slice`, `eq`, `masked_fill`, `contiguous`). IR only — no execution,
  no shape inference, no lowering past this level.

- **`python/tir_inductor/`** — the backend. Register it with
  `torch.compile(model, backend=tir_backend)`. For each compiled region
  TorchDynamo hands it the FX graph; it walks the nodes and builds TIR
  **through the TIR Python bindings** — types via the structural type builder,
  one `tir.torch.<op>(ctx, ...)` per node, assembled into a block. It returns
  the original `forward` so the model still runs.

The `torch` dialect is linked into `tir-capi` and registered in every context
the bindings create, so `tir.torch.*` constructors and the tensor type are
available from Python automatically.

## Run the demo

```sh
cargo build -p tir-capi          # build the C ABI the bindings load
pip install torch                # CPU build is enough
python3 torch-inductor/python/demo_nanogpt.py
```

This builds a small nanoGPT (the simplest form of
[karpathy/nanoGPT](https://github.com/karpathy/nanoGPT/blob/master/model.py) —
no flash attention, no dropout), compiles it with the TIR backend, and prints
the lowered `torch`-dialect IR, e.g.:

```
%33 = torch.embedding %1, %0 : !torch.tensor<f32, 1, 4, 8>
%37 = torch.linear %36, %5 : !torch.tensor<f32, 1, 4, 24>
%39 = torch.split %38 {dim = 2, size = 8, index = 0} : !torch.tensor<f32, 1, 4, 8>
%49 = torch.matmul %45, %48 : !torch.tensor<f32, 1, 2, 4, 4>
%53 = torch.masked_fill %50, %52 {value = "-inf"} : !torch.tensor<f32, 1, 2, 4, 4>
%54 = torch.softmax %53 {dim = -1} : !torch.tensor<f32, 1, 2, 4, 4>
```

## Test

```sh
cargo test -p tir-torch          # dialect parse/print round-trip
```
