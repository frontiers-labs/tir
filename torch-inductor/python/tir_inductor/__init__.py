"""A Torch Inductor-style backend that lowers a TorchDynamo FX graph to TIR.

Register it with ``torch.compile(model, backend=tir_backend)``. On each compiled
region TorchDynamo hands us the FX graph of the traced forward pass; we walk it
node by node and build `torch`-dialect TIR through the TIR Python bindings: a
region with one block whose arguments are the model inputs, and one structurally
constructed op (``tir.torch.embedding(ctx, ...)`` and friends) per FX node. The
backend returns the original ``gm.forward`` so the model still runs — it produces
IR, it does not execute it.

The lowering of the most recent compile is on ``tir_backend``: ``ctx`` (the TIR
context), ``block`` (the lowered ops) and ``result`` (the returned value).
"""

import os
import sys

# Make the TIR Python bindings importable when running from a source checkout.
_HERE = os.path.dirname(os.path.abspath(__file__))
_PYTHON = os.path.abspath(os.path.join(_HERE, "..", "..", "..", "utils", "python"))
if _PYTHON not in sys.path:
    sys.path.insert(0, _PYTHON)

import tir  # noqa: E402
from tir._capi import TYPEARG_I64, TYPEARG_U32  # noqa: E402

__all__ = ["TirBackend", "tir_backend", "lower_graph_module"]

# dtype name -> the stable code understood by the Rust `torch.tensor` builder.
_DTYPE_CODE = {
    "torch.float32": 0,
    "torch.float16": 1,
    "torch.bfloat16": 2,
    "torch.float64": 3,
    "torch.int64": 4,
    "torch.int32": 5,
    "torch.bool": 6,
}


def _target_name(node):
    """The op name of an FX node: ``call_method`` targets are strings,
    ``call_function`` targets are callables."""
    t = node.target
    return t if isinstance(t, str) else getattr(t, "__name__", str(t))


def _val(node):
    meta = getattr(node, "meta", {})
    return meta.get("example_value", meta.get("val"))


def _is_param(node):
    return "parameters" in node.name or "buffers" in node.name


def _attr_path(name):
    """Turn a lifted-parameter placeholder name into its dotted module path."""
    s = name.rstrip("_")
    s = s.replace("l_self_modules_", "").replace("l_self_", "")
    s = s.replace("_modules_", ".").replace("_parameters_", ".").replace("_buffers_", ".")
    return s


def _slice_spec(index):
    """Render a getitem index (a slice or tuple of slices) as Python-ish text."""

    def one(s):
        if isinstance(s, slice):
            lo = "" if s.start is None else s.start
            hi = "" if s.stop is None else s.stop
            return f"{lo}:{hi}" if s.step is None else f"{lo}:{hi}:{s.step}"
        return str(s)

    items = index if isinstance(index, tuple) else (index,)
    return "[" + ", ".join(one(s) for s in items) + "]"


class _Lowering:
    """Builds the TIR for one FX graph through the bindings."""

    def __init__(self, ctx, gm):
        self.ctx = ctx
        self.gm = gm
        self.torch = tir.torch
        self.value = {}  # fx node -> tir.Value

    def tensor_type(self, node):
        v = _val(node)
        if v is None or not hasattr(v, "shape"):
            raise NotImplementedError(f"node {node} has no tensor metadata to type")
        code = _DTYPE_CODE.get(str(v.dtype))
        if code is None:
            raise NotImplementedError(f"unsupported dtype {v.dtype}")
        args = [(TYPEARG_U32, code)] + [(TYPEARG_I64, int(d)) for d in v.shape]
        return self.ctx._build_type("torch", "tensor", args)

    def op(self, node, op_name, operands, **attrs):
        """Construct one `torch.<op_name>` op, append it, and record its result."""
        ctor = getattr(self.torch, op_name)
        built = ctor(self.ctx, *operands, result_type=self.tensor_type(node), **attrs)
        self.block.append(built)
        self.value[node] = built.results[0]
        return built

    def operand(self, node):
        return self.value[node]

    def run(self):
        g = self.gm.graph
        inputs = [n for n in g.nodes if n.op == "placeholder" and not _is_param(n)]
        params = [n for n in g.nodes if n.op == "placeholder" and _is_param(n)]

        self.region = self.ctx.create_region()
        self.block = self.ctx.create_block([self.tensor_type(i) for i in inputs])
        self.region.append_block(self.block)
        for i, arg in zip(inputs, self.block.args):
            self.value[i] = arg

        # Parameters/buffers become `torch.get_attr` ops at the top of the block.
        for p in params:
            self.op(p, "get_attr", (), name=_attr_path(p.name))

        result = None
        for n in g.nodes:
            if n.op == "placeholder":
                continue
            if n.op == "output":
                result = self.operand(n.args[0][0])
                continue
            self._lower(n)

        ret = getattr(tir.builtin, "return_")(self.ctx, result)
        self.block.append(ret)
        return self.block, result

    def _lower(self, n):
        target = _target_name(n)
        a, kw = n.args, n.kwargs

        if target == "split":
            return  # container; its getitem users materialize each chunk
        if target == "getitem":
            src = n.args[0]
            if _target_name(src) == "split":
                self.op(
                    n,
                    "split",
                    (self.operand(src.args[0]),),
                    dim=src.kwargs.get("dim", 0),
                    size=src.args[1],
                    index=a[1],
                )
            else:
                self.op(n, "slice", (self.operand(src),), spec=_slice_spec(a[1]))
            return

        if target == "arange":
            self.op(n, "arange", (), n=a[1])
        elif target == "embedding":
            self.op(n, "embedding", (self.operand(a[1]), self.operand(a[0])))
        elif target == "add":
            self.op(n, "add", (self.operand(a[0]), self.operand(a[1])))
        elif target == "matmul":
            self.op(n, "matmul", (self.operand(a[0]), self.operand(a[1])))
        elif target == "mul":
            self.op(n, "mul_scalar", (self.operand(a[0]),), value=str(a[1]))
        elif target == "linear":
            lin = self.op(n, "linear", (self.operand(a[0]), self.operand(a[1])))
            if len(a) > 2 and a[2] is not None:
                # Fold the bias into an explicit broadcast add over the linear.
                add = self.torch.add(
                    self.ctx,
                    lin.results[0],
                    self.operand(a[2]),
                    result_type=self.tensor_type(n),
                )
                self.block.append(add)
                self.value[n] = add.results[0]
        elif target == "layer_norm":
            self.op(n, "layer_norm", (self.operand(a[0]), self.operand(a[2]), self.operand(a[3])))
        elif target == "gelu":
            self.op(n, "gelu", (self.operand(a[0]),))
        elif target == "softmax":
            self.op(n, "softmax", (self.operand(a[0]),), dim=kw["dim"])
        elif target == "transpose":
            self.op(n, "transpose", (self.operand(a[0]),), dim0=a[1], dim1=a[2])
        elif target == "view":
            self.op(n, "view", (self.operand(a[0]),))
        elif target == "contiguous":
            self.op(n, "contiguous", (self.operand(a[0]),))
        elif target == "__eq__":
            self.op(n, "eq", (self.operand(a[0]),), value=str(a[1]))
        elif target == "masked_fill":
            self.op(n, "masked_fill", (self.operand(a[0]), self.operand(a[1])), value=str(a[2]))
        else:
            raise NotImplementedError(f"no TIR lowering for FX target {target!r}")


def lower_graph_module(ctx, gm):
    """Lower a TorchDynamo ``GraphModule`` into a TIR block. Returns
    ``(block, result_value)``."""
    return _Lowering(ctx, gm).run()


class TirBackend:
    """A ``torch.compile`` backend that lowers each FX graph to TIR."""

    def __init__(self):
        self.ctx = None
        self.block = None
        self.result = None

    def __call__(self, gm, example_inputs):
        self.ctx = tir.Context()
        self.block, self.result = lower_graph_module(self.ctx, gm)
        return gm.forward

    def text(self):
        """The lowered ops, one per line, as printed by TIR."""
        return "\n".join(op.to_string().rstrip("\n") for op in self.block.ops)


tir_backend = TirBackend()
