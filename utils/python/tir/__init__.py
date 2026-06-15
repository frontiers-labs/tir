"""Pythonic API for TIR, built on the generic C ABI.

The generic verbs (parse, print, run pipelines, inspect, mutate) are hand-written
here; the typed per-op constructors in ``_ops`` are generated from the schema, so
``tir.builtin.addi(...)`` and friends appear automatically as ops are added.
"""

import ctypes

from . import _ops
from ._capi import INVALID, lib

__all__ = ["Context", "Op", "Value", "Region", "Block", "TirError"]


class TirError(Exception):
    pass


def _last_error():
    msg = lib.tir_last_error()
    return msg.decode() if msg else "unknown error"


def _take_str(owned):
    """Decode and free an owned C string, or return None for null."""
    if not owned:
        return None
    text = ctypes.cast(owned, ctypes.c_char_p).value
    lib.tir_string_free(owned)
    return text.decode() if text is not None else None


def _vid(value):
    return value.id if isinstance(value, Value) else int(value)


def _attr_literal(value):
    if isinstance(value, bool):
        raise TirError("boolean attributes are not supported by the textual builder")
    if isinstance(value, int):
        return str(value)
    if isinstance(value, str):
        return '"%s"' % value
    return str(value)


class Value:
    def __init__(self, ctx, id):
        self._ctx = ctx
        self.id = id

    @property
    def type(self):
        ty = lib.tir_value_type(self._ctx._p, self.id)
        return None if ty == INVALID else _take_str(lib.tir_type_to_string(self._ctx._p, ty))

    def __repr__(self):
        return "%%%d" % self.id


class Op:
    def __init__(self, ctx, id):
        self._ctx = ctx
        self.id = id

    @property
    def name(self):
        return _take_str(lib.tir_op_name(self._ctx._p, self.id))

    @property
    def dialect(self):
        return _take_str(lib.tir_op_dialect(self._ctx._p, self.id))

    @property
    def operands(self):
        n = lib.tir_op_num_operands(self._ctx._p, self.id)
        return [Value(self._ctx, lib.tir_op_operand(self._ctx._p, self.id, i)) for i in range(n)]

    @property
    def results(self):
        n = lib.tir_op_num_results(self._ctx._p, self.id)
        return [Value(self._ctx, lib.tir_op_result(self._ctx._p, self.id, i)) for i in range(n)]

    @property
    def regions(self):
        n = lib.tir_op_num_regions(self._ctx._p, self.id)
        return [Region(self._ctx, lib.tir_op_region(self._ctx._p, self.id, i)) for i in range(n)]

    @property
    def attributes(self):
        n = lib.tir_op_num_attributes(self._ctx._p, self.id)
        out = []
        for i in range(n):
            name = _take_str(lib.tir_op_attribute_name(self._ctx._p, self.id, i))
            kind = lib.tir_op_attribute_kind(self._ctx._p, self.id, i)
            out.append((name, kind))
        return out

    def attribute_int(self, index):
        holder = ctypes.c_int64(0)
        if lib.tir_op_attribute_int(self._ctx._p, self.id, index, ctypes.byref(holder)):
            return holder.value
        return None

    def to_string(self):
        return _take_str(lib.tir_op_to_string(self._ctx._p, self.id))

    def walk(self):
        """Yield this op and every op nested in its regions, depth-first."""
        yield self
        for region in self.regions:
            for block in region.blocks:
                for op in block.ops:
                    yield from op.walk()

    def __repr__(self):
        return "<Op %s.%s #%d>" % (self.dialect, self.name, self.id)


class Region:
    def __init__(self, ctx, id):
        self._ctx = ctx
        self.id = id

    @property
    def blocks(self):
        n = lib.tir_region_num_blocks(self._ctx._p, self.id)
        return [Block(self._ctx, lib.tir_region_block(self._ctx._p, self.id, i)) for i in range(n)]

    def append_block(self, block):
        if not lib.tir_region_append_block(self._ctx._p, self.id, block.id):
            raise TirError(_last_error())


class Block:
    def __init__(self, ctx, id):
        self._ctx = ctx
        self.id = id

    @property
    def ops(self):
        n = lib.tir_block_num_ops(self._ctx._p, self.id)
        return [Op(self._ctx, lib.tir_block_op(self._ctx._p, self.id, i)) for i in range(n)]

    @property
    def args(self):
        n = lib.tir_block_num_args(self._ctx._p, self.id)
        return [Value(self._ctx, lib.tir_block_arg(self._ctx._p, self.id, i)) for i in range(n)]

    def append(self, op):
        if not lib.tir_block_append_op(self._ctx._p, self.id, op.id):
            raise TirError(_last_error())

    def insert(self, index, op):
        if not lib.tir_block_insert_op(self._ctx._p, self.id, index, op.id):
            raise TirError(_last_error())

    def remove(self, op):
        if not lib.tir_block_remove_op(self._ctx._p, self.id, op.id):
            raise TirError(_last_error())


class Context:
    def __init__(self):
        self._p = lib.tir_context_create()
        if not self._p:
            raise TirError("failed to create context")

    def close(self):
        if self._p:
            lib.tir_context_destroy(self._p)
            self._p = None

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()

    def __del__(self):
        self.close()

    def parse_module(self, text):
        data = text.encode()
        op = lib.tir_parse_module(self._p, data, len(data))
        if op == INVALID:
            raise TirError(_last_error())
        return Op(self, op)

    def parse_op(self, text):
        data = text.encode()
        op = lib.tir_parse_op(self._p, data, len(data))
        if op == INVALID:
            raise TirError(_last_error())
        return Op(self, op)

    def run_pipeline(self, root, spec):
        pm = lib.tir_pipeline_parse(spec.encode())
        if not pm:
            raise TirError(_last_error())
        try:
            if not lib.tir_pipeline_run(pm, self._p, root.id):
                raise TirError(_last_error())
        finally:
            lib.tir_pipeline_destroy(pm)

    def parse_type(self, spec):
        ty = lib.tir_type_parse(self._p, spec.encode())
        if ty == INVALID:
            raise TirError(_last_error())
        return ty

    def create_region(self):
        rid = lib.tir_region_create(self._p)
        if rid == INVALID:
            raise TirError(_last_error())
        return Region(self, rid)

    def create_block(self, arg_type_ids=()):
        ids = list(arg_type_ids)
        arr = (ctypes.c_uint32 * len(ids))(*ids) if ids else None
        bid = lib.tir_block_create(self._p, arr, len(ids))
        if bid == INVALID:
            raise TirError(_last_error())
        return Block(self, bid)

    def _build_op(self, dialect, name, operands, result_type, attrs):
        parts = ["%s.%s" % (dialect, name)]
        if operands:
            parts.append(", ".join("%%%d" % _vid(o) for o in operands))
        if attrs:
            body = ", ".join("%s = %s" % (k, _attr_literal(v)) for k, v in attrs.items())
            parts.append("{%s}" % body)
        if result_type is not None:
            parts.append(": %s" % result_type)
        return self.parse_op(" ".join(parts))

    @staticmethod
    def schema_json():
        return _take_str(lib.tir_schema_json())


# Expose the generated per-dialect constructor classes as `tir.<dialect>`.
for _name in dir(_ops):
    _obj = getattr(_ops, _name)
    if isinstance(_obj, type) and not _name.startswith("_"):
        globals()[_name] = _obj
        __all__.append(_name)
