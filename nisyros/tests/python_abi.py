import ctypes as c
import sys


class Header(c.Structure):
    _fields_ = [
        ("struct_size", c.c_uint32),
        ("abi_version", c.c_uint32),
        ("extensions", c.c_void_p),
    ]


class DeviceId(c.Structure):
    _fields_ = [
        ("epoch", c.c_uint64),
        ("index", c.c_uint32),
        ("backend", c.c_uint32),
    ]


class DeviceInfo(c.Structure):
    _fields_ = [
        ("header", Header),
        ("backend", c.c_uint32),
        ("api_version", c.c_uint32),
        ("vendor_id", c.c_uint32),
        ("device_id", c.c_uint32),
        ("queue_family_count", c.c_uint32),
        ("strict_profile_supported", c.c_uint32),
        ("name", c.c_char * 256),
        ("device_uuid", c.c_uint8 * 16),
        ("driver_uuid", c.c_uint8 * 16),
        ("driver_id", c.c_uint32),
        ("device_type", c.c_uint32),
        ("driver_name", c.c_char * 256),
        ("node_index", c.c_uint32),
        ("is_local", c.c_uint32),
    ]


class BackendApi(c.Structure):
    _fields_ = [
        ("struct_size", c.c_uint32),
        ("abi_version", c.c_uint32),
        ("get_extension", c.c_void_p),
    ]


class DiagnosticInfo(c.Structure):
    _fields_ = [
        ("header", Header),
        ("status", c.c_int32),
        ("backend_code", c.c_int32),
    ]


library = c.CDLL(sys.argv[1])
library.nisyros_backend_get_api.argtypes = [c.c_uint32, c.c_uint32, c.c_uint32, c.c_void_p]
library.nisyros_backend_get_api.restype = c.c_int32
library.nisyros_diagnostic_get.argtypes = [
    c.POINTER(DiagnosticInfo), c.POINTER(c.c_uint32), c.c_void_p,
]
library.nisyros_diagnostic_get.restype = c.c_int32
library.nisyros_cluster_open_local.argtypes = [c.POINTER(c.c_void_p)]
library.nisyros_cluster_open_local.restype = c.c_int32
library.nisyros_cluster_devices.argtypes = [
    c.c_void_p,
    c.POINTER(c.c_uint32),
    c.POINTER(DeviceId),
]
library.nisyros_cluster_devices.restype = c.c_int32
library.nisyros_cluster_device_info.argtypes = [
    c.c_void_p,
    DeviceId,
    c.POINTER(DeviceInfo),
]
library.nisyros_cluster_device_info.restype = c.c_int32
library.nisyros_cluster_close.argtypes = [c.c_void_p]
library.nisyros_cluster_close.restype = c.c_int32

backend = BackendApi()
assert library.nisyros_backend_get_api(1, 1, 8, c.byref(backend)) == 1
assert backend.struct_size == c.sizeof(BackendApi) and not backend.get_extension
assert library.nisyros_backend_get_api(1, 1, c.sizeof(backend), c.byref(backend)) == 0
assert backend.abi_version == 1 and backend.get_extension
assert library.nisyros_backend_get_api(1, 999, c.sizeof(backend), c.byref(backend)) == -2
assert not backend.get_extension

diagnostic = DiagnosticInfo(Header(c.sizeof(DiagnosticInfo), 1, None), 0, 0)
required = c.c_uint32()
assert library.nisyros_diagnostic_get(c.byref(diagnostic), c.byref(required), None) == 0
assert diagnostic.status == 0 and diagnostic.backend_code == 0 and required.value == 1

cluster = c.c_void_p()
assert library.nisyros_cluster_open_local(c.byref(cluster)) == 0
count = c.c_uint32()
assert library.nisyros_cluster_devices(cluster, c.byref(count), None) == 0
assert count.value > 0
ids = (DeviceId * count.value)()
assert library.nisyros_cluster_devices(cluster, c.byref(count), ids) == 0
info = DeviceInfo()
info.header = Header(c.sizeof(DeviceInfo), 999, None)
assert library.nisyros_cluster_device_info(cluster, ids[0], c.byref(info)) == -1
info.header.abi_version = 1
assert library.nisyros_cluster_device_info(cluster, ids[0], c.byref(info)) == 0
assert info.backend == 1
assert info.queue_family_count > 0
assert info.driver_name
assert info.node_index == 0 and info.is_local == 1
assert info.strict_profile_supported in (0, 1)
assert library.nisyros_cluster_close(cluster) == 0
print(info.name.decode(), "strict=", info.strict_profile_supported)
