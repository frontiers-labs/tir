"""Low-level ctypes bindings to the TIR C ABI (libtir_capi).

This is the stable hand-written layer over the generic C ABI; it does not grow
when ops are added. The library is located via the TIR_CAPI_LIBRARY environment
variable, or by searching the cargo target directory.
"""

import ctypes
import glob
import os

INVALID = 0xFFFFFFFF


def _locate():
    env = os.environ.get("TIR_CAPI_LIBRARY")
    if env:
        return env
    here = os.path.dirname(os.path.abspath(__file__))
    root = os.path.abspath(os.path.join(here, "..", "..", ".."))
    for profile in ("debug", "release"):
        hits = glob.glob(os.path.join(root, "target", profile, "libtir_capi.*"))
        hits = [h for h in hits if h.endswith((".so", ".dylib", ".dll"))]
        if hits:
            return hits[0]
    raise OSError("could not locate libtir_capi; set TIR_CAPI_LIBRARY")


lib = ctypes.CDLL(_locate())

_u32 = ctypes.c_uint32
_usize = ctypes.c_size_t
_cstr = ctypes.c_char_p
_ptr = ctypes.c_void_p
_bool = ctypes.c_bool
_i32 = ctypes.c_int32
_i64p = ctypes.POINTER(ctypes.c_int64)
_u32p = ctypes.POINTER(_u32)
# Owned strings are returned as void_p so the caller can free them explicitly.
_owned = _ptr

# name -> (restype, [argtypes])
_SPEC = {
    "tir_context_create": (_ptr, []),
    "tir_context_destroy": (None, [_ptr]),
    "tir_last_error": (_cstr, []),
    "tir_string_free": (None, [_ptr]),
    "tir_schema_json": (_owned, []),
    "tir_parse_module": (_u32, [_ptr, _cstr, _usize]),
    "tir_parse_op": (_u32, [_ptr, _cstr, _usize]),
    "tir_op_to_string": (_owned, [_ptr, _u32]),
    "tir_pipeline_parse": (_ptr, [_cstr]),
    "tir_pipeline_run": (_bool, [_ptr, _ptr, _u32]),
    "tir_pipeline_destroy": (None, [_ptr]),
    "tir_op_name": (_owned, [_ptr, _u32]),
    "tir_op_dialect": (_owned, [_ptr, _u32]),
    "tir_op_num_operands": (_usize, [_ptr, _u32]),
    "tir_op_operand": (_u32, [_ptr, _u32, _usize]),
    "tir_op_num_results": (_usize, [_ptr, _u32]),
    "tir_op_result": (_u32, [_ptr, _u32, _usize]),
    "tir_op_num_regions": (_usize, [_ptr, _u32]),
    "tir_op_region": (_u32, [_ptr, _u32, _usize]),
    "tir_value_type": (_u32, [_ptr, _u32]),
    "tir_region_num_blocks": (_usize, [_ptr, _u32]),
    "tir_region_block": (_u32, [_ptr, _u32, _usize]),
    "tir_block_num_ops": (_usize, [_ptr, _u32]),
    "tir_block_op": (_u32, [_ptr, _u32, _usize]),
    "tir_block_num_args": (_usize, [_ptr, _u32]),
    "tir_block_arg": (_u32, [_ptr, _u32, _usize]),
    "tir_type_parse": (_u32, [_ptr, _cstr]),
    "tir_type_to_string": (_owned, [_ptr, _u32]),
    "tir_op_num_attributes": (_usize, [_ptr, _u32]),
    "tir_op_attribute_name": (_owned, [_ptr, _u32, _usize]),
    "tir_op_attribute_kind": (_i32, [_ptr, _u32, _usize]),
    "tir_op_attribute_int": (_bool, [_ptr, _u32, _usize, _i64p]),
    "tir_region_create": (_u32, [_ptr]),
    "tir_block_create": (_u32, [_ptr, _u32p, _usize]),
    "tir_region_append_block": (_bool, [_ptr, _u32, _u32]),
    "tir_block_append_op": (_bool, [_ptr, _u32, _u32]),
    "tir_block_insert_op": (_bool, [_ptr, _u32, _usize, _u32]),
    "tir_block_remove_op": (_bool, [_ptr, _u32, _u32]),
    "tir_supported_targets": (_owned, []),
    "tir_context_register_target": (_bool, [_ptr, _cstr, _cstr, _cstr]),
    "tir_context_run_target_pipeline": (_bool, [_ptr, _u32, _cstr, _cstr, _cstr, _i32]),
}

for _name, (_res, _args) in _SPEC.items():
    _fn = getattr(lib, _name)
    _fn.restype = _res
    _fn.argtypes = _args
