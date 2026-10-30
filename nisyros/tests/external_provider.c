#define _POSIX_C_SOURCE 200809L
#include "nisyros_plugin.h"
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define TEST_BACKEND 0x80000001u

typedef struct { void *data; uint64_t bytes; } allocation;
typedef struct { uint64_t value; } timeline;
typedef struct { nisyros_plugin_command_op *ops; uint32_t count; } command;

static nisyros_status instance_open(const nisyros_plugin_services *services, void **out) {
    if (!services || !out || services->abi_version != NISYROS_PLUGIN_SERVICES_VERSION)
        return NISYROS_INVALID_ARGUMENT;
    *out = malloc(1);
    return *out ? NISYROS_SUCCESS : NISYROS_OUT_OF_MEMORY;
}
static void instance_close(void *instance) { free(instance); }
static nisyros_status device_count(void *instance, uint32_t *out) {
    if (!instance || !out) return NISYROS_INVALID_ARGUMENT;
    *out = 1;
    return NISYROS_SUCCESS;
}
static nisyros_status device_info(void *instance, uint32_t index,
                                  nisyros_plugin_device_info *out) {
    if (!instance || index || !out || out->struct_size < sizeof(*out) ||
        out->abi_version != NISYROS_PLUGIN_ABI_VERSION)
        return NISYROS_INVALID_ARGUMENT;
    out->backend = TEST_BACKEND;
    out->api_version = 1;
    out->device_type = NISYROS_DEVICE_TYPE_CPU;
    out->strict_profile_supported = 1;
    out->queue_family_count = 1;
    out->memory_heap_count = 1;
    out->max_grid[0] = out->max_grid[1] = out->max_grid[2] = 65535;
    out->timestamp_period_ns = 1.0f;
    memcpy(out->name, "Independent test provider", 26);
    memcpy(out->driver_name, "Nisyros test provider", 22);
    out->device_uuid[0] = 0xa5;
    out->driver_uuid[0] = 0x5a;
    return NISYROS_SUCCESS;
}
static nisyros_status queue_family_info(void *instance, uint32_t device_index,
                                        uint32_t family, nisyros_plugin_queue_family_info *out) {
    if (!instance || device_index || family || !out) return NISYROS_INVALID_ARGUMENT;
    *out = (nisyros_plugin_queue_family_info){NISYROS_QUEUE_COMPUTE | NISYROS_QUEUE_TRANSFER, 1, 0};
    return NISYROS_SUCCESS;
}
static nisyros_status memory_heap_info(void *instance, uint32_t device_index,
                                       uint32_t heap, nisyros_plugin_memory_heap_info *out) {
    if (!instance || device_index || heap || !out) return NISYROS_INVALID_ARGUMENT;
    *out = (nisyros_plugin_memory_heap_info){1ull << 30, NISYROS_HEAP_DEVICE_LOCAL, 0};
    return NISYROS_SUCCESS;
}
static nisyros_status cooperative_matrix(void *instance, uint32_t device_index,
                                          uint32_t tuple, nisyros_cooperative_matrix_tuple *out) {
    (void)instance; (void)device_index; (void)tuple; (void)out;
    return NISYROS_INVALID_ARGUMENT;
}
static nisyros_status device_open(void *instance, uint32_t index, uint32_t request_count,
                                  const nisyros_plugin_queue_request *requests, void **out) {
    if (!instance || index || request_count != 1 || !requests || !out ||
        requests[0].family || requests[0].count != 1)
        return NISYROS_INVALID_ARGUMENT;
    *out = malloc(1);
    return *out ? NISYROS_SUCCESS : NISYROS_OUT_OF_MEMORY;
}
static void device_close(void *device) { free(device); }
static nisyros_status allocation_create(void *device, const nisyros_plugin_alloc_request *request,
                                        void **out, nisyros_plugin_allocation_info *info) {
    if (!device || !request || !out || !info || !request->bytes ||
        !request->alignment || (request->alignment & (request->alignment - 1)))
        return NISYROS_INVALID_ARGUMENT;
    allocation *a = calloc(1, sizeof(*a));
    if (!a) return NISYROS_OUT_OF_MEMORY;
    size_t alignment = request->alignment < sizeof(void *) ? sizeof(void *) : request->alignment;
    if (request->bytes > SIZE_MAX || posix_memalign(&a->data, alignment, request->bytes)) {
        free(a);
        return NISYROS_OUT_OF_MEMORY;
    }
    memset(a->data, 0, request->bytes);
    a->bytes = request->bytes;
    *info = (nisyros_plugin_allocation_info){
        (uint64_t)(uintptr_t)a->data, a->bytes, a->bytes, 1ull << 30, 0, 0,
        NISYROS_MEMORY_DEVICE_LOCAL | NISYROS_MEMORY_HOST_VISIBLE |
        NISYROS_MEMORY_HOST_COHERENT, 1};
    *out = a;
    return NISYROS_SUCCESS;
}
static void allocation_destroy(void *device, void *handle) {
    (void)device;
    if (handle) { allocation *a = handle; free(a->data); free(a); }
}
static nisyros_status allocation_map(void *device, void *handle, uint64_t address, void **out) {
    (void)device;
    if (!handle || !out) return NISYROS_INVALID_ARGUMENT;
    allocation *a = handle;
    uint64_t base = (uint64_t)(uintptr_t)a->data;
    if (address < base || address - base >= a->bytes) return NISYROS_INVALID_ARGUMENT;
    *out = (char *)a->data + (address - base);
    return NISYROS_SUCCESS;
}
static void allocation_unmap(void *device, void *handle) { (void)device; (void)handle; }
static nisyros_status allocation_visibility(void *device, void *handle, uint32_t flush) {
    (void)device; (void)flush;
    return handle ? NISYROS_SUCCESS : NISYROS_INVALID_ARGUMENT;
}
static nisyros_status allocation_register_host(void *device, void *host, uint64_t bytes,
                                               void **out, nisyros_plugin_allocation_info *info) {
    (void)device; (void)host; (void)bytes; (void)out; (void)info;
    return NISYROS_UNSUPPORTED;
}
static nisyros_status allocation_export_fd(void *device, void *handle, int32_t *out_fd) {
    (void)device; (void)handle; (void)out_fd;
    return NISYROS_UNSUPPORTED;
}
static nisyros_status allocation_import_fd(void *device, void *source, uint64_t offset,
                                            uint64_t bytes, int32_t fd, void **out,
                                            nisyros_plugin_allocation_info *info) {
    (void)device; (void)source; (void)offset; (void)bytes; (void)out; (void)info;
    if (fd >= 0) close(fd);
    return NISYROS_UNSUPPORTED;
}
static nisyros_status device_wait_idle(void *device, int32_t *code) {
    (void)code;
    return device ? NISYROS_SUCCESS : NISYROS_INVALID_ARGUMENT;
}
static nisyros_status timeline_create(void *device, uint32_t exportable, void **out, int32_t *code) {
    (void)code;
    if (!device || !out || exportable) return NISYROS_UNSUPPORTED;
    *out = calloc(1, sizeof(timeline));
    return *out ? NISYROS_SUCCESS : NISYROS_OUT_OF_MEMORY;
}
static nisyros_status timeline_import_fd(void *device, int32_t fd, void **out, int32_t *code) {
    (void)device; (void)out; (void)code;
    if (fd >= 0) close(fd);
    return NISYROS_UNSUPPORTED;
}
static void timeline_destroy(void *device, void *handle) { (void)device; free(handle); }
static nisyros_status timeline_query(void *handle, uint64_t *value, int32_t *code) {
    (void)code;
    if (!handle || !value) return NISYROS_INVALID_ARGUMENT;
    *value = ((timeline *)handle)->value;
    return NISYROS_SUCCESS;
}
static nisyros_status timeline_wait(void *handle, uint64_t value, uint64_t timeout,
                                    uint32_t *completed, int32_t *code) {
    (void)timeout; (void)code;
    if (!handle || !completed) return NISYROS_INVALID_ARGUMENT;
    *completed = ((timeline *)handle)->value >= value;
    return NISYROS_SUCCESS;
}
static nisyros_status timeline_signal(void *handle, uint64_t value, int32_t *code) {
    (void)code;
    if (!handle || value <= ((timeline *)handle)->value) return NISYROS_INVALID_ARGUMENT;
    ((timeline *)handle)->value = value;
    return NISYROS_SUCCESS;
}
static nisyros_status timeline_export_fd(void *handle, int32_t *fd, int32_t *code) {
    (void)handle; (void)fd; (void)code;
    return NISYROS_UNSUPPORTED;
}
static nisyros_status pipeline_import(void *device, const uint8_t *image, uint64_t bytes,
                                      void **out, int32_t *code) {
    (void)device; (void)image; (void)bytes; (void)out; (void)code;
    return NISYROS_UNSUPPORTED;
}
static void pipeline_destroy(void *device, void *pipeline) { (void)device; (void)pipeline; }
static nisyros_status query_create(void *device, void **out, int32_t *code) {
    (void)device; (void)out; (void)code;
    return NISYROS_UNSUPPORTED;
}
static void query_destroy(void *device, void *query) { (void)device; (void)query; }
static nisyros_status command_create(void *device, uint32_t family, void **out, int32_t *code) {
    (void)code;
    if (!device || family || !out) return NISYROS_INVALID_ARGUMENT;
    *out = calloc(1, sizeof(command));
    return *out ? NISYROS_SUCCESS : NISYROS_OUT_OF_MEMORY;
}
static void command_destroy(void *device, void *handle) {
    (void)device;
    if (handle) { command *c = handle; free(c->ops); free(c); }
}
static nisyros_status command_record(void *handle, uint32_t count,
                                     const nisyros_plugin_command_op *ops, int32_t *code) {
    (void)code;
    if (!handle || (count && !ops)) return NISYROS_INVALID_ARGUMENT;
    command *c = handle;
    nisyros_plugin_command_op *copy = count ? malloc(count * sizeof(*copy)) : NULL;
    if (count && !copy) return NISYROS_OUT_OF_MEMORY;
    if (count) memcpy(copy, ops, count * sizeof(*copy));
    free(c->ops);
    c->ops = copy;
    c->count = count;
    return NISYROS_SUCCESS;
}
static nisyros_status queue_submit(void *device, uint32_t queue_index, uint32_t count,
                                   const nisyros_plugin_submit_segment *segments, int32_t *code) {
    (void)code;
    if (!device || queue_index || !count || !segments) return NISYROS_INVALID_ARGUMENT;
    for (uint32_t i = 0; i < count; ++i) {
        const nisyros_plugin_submit_segment *segment = &segments[i];
        for (uint32_t j = 0; j < segment->wait_count; ++j)
            if (((timeline *)segment->waits[j].timeline)->value < segment->waits[j].value)
                return NISYROS_SUBMISSION_FAILED;
        command *c = segment->command;
        if (c) for (uint32_t j = 0; j < c->count; ++j) {
            const nisyros_plugin_command_op *op = &c->ops[j];
            if (op->kind == NISYROS_PLUGIN_OP_FILL) {
                uint8_t pattern[4];
                uint32_t word = op->args[2];
                memcpy(pattern, &word, sizeof(pattern));
                uint8_t *dst = (void *)(uintptr_t)op->args[0];
                for (uint64_t k = 0; k < op->args[1]; ++k) dst[k] = pattern[k % 4];
            } else if (op->kind == NISYROS_PLUGIN_OP_COPY) {
                memcpy((void *)(uintptr_t)op->args[0], (void *)(uintptr_t)op->args[1], op->args[2]);
            } else if (op->kind != NISYROS_PLUGIN_OP_BARRIER_ALL &&
                       op->kind != NISYROS_PLUGIN_OP_BARRIER_RANGE &&
                       op->kind != NISYROS_PLUGIN_OP_BARRIER_SCOPED) {
                return NISYROS_UNSUPPORTED;
            }
        }
        for (uint32_t j = 0; j < segment->signal_count; ++j) {
            timeline *t = segment->signals[j].timeline;
            t->value = segment->signals[j].value;
        }
    }
    return NISYROS_SUCCESS;
}
static nisyros_status instance_native_handles(void *instance, uint32_t index,
                                               void **native_instance, void **physical) {
    (void)instance; (void)index; (void)native_instance; (void)physical;
    return NISYROS_UNSUPPORTED;
}
static nisyros_status device_native_handles(void *device, uint32_t index,
                                             void **native_device, void **native_queue) {
    (void)device; (void)index; (void)native_device; (void)native_queue;
    return NISYROS_UNSUPPORTED;
}

NISYROS_API nisyros_status NISYROS_CALL
nisyros_plugin_get_api(uint32_t version, const nisyros_plugin_services *services,
                       uint32_t table_size, void *table) {
    if (!table || table_size < 8) return NISYROS_INVALID_ARGUMENT;
    memset(table, 0, table_size < sizeof(nisyros_plugin_api) ? table_size : sizeof(nisyros_plugin_api));
    if (version != NISYROS_PLUGIN_ABI_VERSION) return NISYROS_UNSUPPORTED;
    if (!services || services->struct_size < sizeof(*services) ||
        services->abi_version != NISYROS_PLUGIN_SERVICES_VERSION)
        return NISYROS_INVALID_ARGUMENT;
    nisyros_plugin_api api = {
        .struct_size = sizeof(api), .abi_version = NISYROS_PLUGIN_ABI_VERSION,
        .instance_open = instance_open, .instance_close = instance_close,
        .device_count = device_count, .device_info = device_info,
        .queue_family_info = queue_family_info, .memory_heap_info = memory_heap_info,
        .cooperative_matrix = cooperative_matrix, .device_open = device_open,
        .device_close = device_close, .allocation_create = allocation_create,
        .allocation_destroy = allocation_destroy, .allocation_map = allocation_map,
        .allocation_unmap = allocation_unmap, .allocation_visibility = allocation_visibility,
        .allocation_register_host = allocation_register_host,
        .allocation_export_fd = allocation_export_fd, .allocation_import_fd = allocation_import_fd,
        .device_wait_idle = device_wait_idle, .timeline_create = timeline_create,
        .timeline_import_fd = timeline_import_fd, .timeline_destroy = timeline_destroy,
        .timeline_query = timeline_query, .timeline_wait = timeline_wait,
        .timeline_signal = timeline_signal, .timeline_export_fd = timeline_export_fd,
        .pipeline_import = pipeline_import, .pipeline_destroy = pipeline_destroy,
        .query_create = query_create, .query_destroy = query_destroy,
        .command_create = command_create, .command_destroy = command_destroy,
        .command_record = command_record, .queue_submit = queue_submit,
        .instance_native_handles = instance_native_handles,
        .device_native_handles = device_native_handles,
    };
    memcpy(table, &api, table_size < sizeof(api) ? table_size : sizeof(api));
    return table_size < sizeof(api) ? NISYROS_INCOMPLETE : NISYROS_SUCCESS;
}
