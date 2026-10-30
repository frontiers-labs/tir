#ifndef NISYROS_PLUGIN_H
#define NISYROS_PLUGIN_H

#include "nisyros.h"

#ifdef __cplusplus
extern "C" {
#endif

#define NISYROS_PLUGIN_ABI_VERSION 1u
#define NISYROS_PLUGIN_SERVICES_VERSION 1u

typedef struct {
    uint32_t struct_size;
    uint32_t abi_version;
    void *context;
    void (NISYROS_CALL *report_error)(void *context, nisyros_status status,
                                      int32_t backend_code, const char *message,
                                      uint32_t message_bytes);
} nisyros_plugin_services;

typedef struct {
    uint32_t struct_size;
    uint32_t abi_version;
    uint32_t backend;
    uint32_t api_version;
    uint32_t vendor_id;
    uint32_t device_id;
    uint32_t device_type;
    uint32_t driver_id;
    uint32_t strict_profile_supported;
    uint32_t queue_family_count;
    uint32_t memory_heap_count;
    uint32_t cooperative_matrix_count;
    uint64_t features;
    uint64_t external_host_alignment;
    uint32_t cooperative_matrix_revision;
    uint32_t shader_bfloat16_revision;
    uint32_t shader_enqueue_revision;
    uint32_t cooperative_matrix_stages;
    uint32_t max_grid[3];
    uint32_t peer_memory_fd;
    uint32_t peer_timeline_fd;
    float timestamp_period_ns;
    char name[256];
    uint8_t device_uuid[16];
    uint8_t driver_uuid[16];
    char driver_name[256];
} nisyros_plugin_device_info;

typedef struct {
    uint32_t flags;
    uint32_t count;
    uint32_t timestamp_valid_bits;
} nisyros_plugin_queue_family_info;

typedef struct {
    uint64_t bytes;
    uint32_t flags;
    uint32_t reserved;
} nisyros_plugin_memory_heap_info;

typedef struct {
    uint32_t family;
    uint32_t count;
    float priority;
} nisyros_plugin_queue_request;

typedef struct {
    uint64_t bytes;
    uint64_t alignment;
    uint32_t required_properties;
    uint32_t preferred_properties;
    uint32_t required_heap;
    uint32_t preferred_heap;
    uint32_t exclusive;
    uint32_t queue_family_count;
    const uint32_t *queue_families;
} nisyros_plugin_alloc_request;

typedef struct {
    uint64_t base;
    uint64_t bytes;
    uint64_t backing_bytes;
    uint64_t heap_bytes;
    uint32_t memory_type_index;
    uint32_t heap_index;
    uint32_t properties;
    uint32_t exclusive;
} nisyros_plugin_allocation_info;

/* Arguments by kind:
 * COPY: dst, src, bytes
 * FILL: dst, bytes, pattern
 * BARRIER_ALL: none
 * BARRIER_RANGE: address, bytes
 * BARRIER_SCOPED: address, bytes, src_stage, src_access, dst_stage,
 *                 dst_access, src_family, dst_family (public Nisyros masks)
 * DISPATCH: pipeline handle, grid_x, grid_y, grid_z, argument address
 * DISPATCH_INDIRECT: pipeline handle, grid address, argument address
 * TIMESTAMP: query handle, destination address
 * Handles are provider-owned opaque pointers represented as uintptr_t.
 */
typedef struct {
    uint32_t kind;
    uint32_t reserved;
    uint64_t args[8];
} nisyros_plugin_command_op;

#define NISYROS_PLUGIN_OP_COPY 1u
#define NISYROS_PLUGIN_OP_FILL 2u
#define NISYROS_PLUGIN_OP_BARRIER_ALL 3u
#define NISYROS_PLUGIN_OP_BARRIER_RANGE 4u
#define NISYROS_PLUGIN_OP_BARRIER_SCOPED 5u
#define NISYROS_PLUGIN_OP_DISPATCH 6u
#define NISYROS_PLUGIN_OP_DISPATCH_INDIRECT 7u
#define NISYROS_PLUGIN_OP_TIMESTAMP 8u

typedef struct {
    void *timeline;
    uint64_t value;
} nisyros_plugin_submit_point;

typedef struct {
    void *command;
    uint32_t wait_count;
    const nisyros_plugin_submit_point *waits;
    uint32_t signal_count;
    const nisyros_plugin_submit_point *signals;
} nisyros_plugin_submit_segment;

typedef struct {
    uint32_t struct_size;
    uint32_t abi_version;
    nisyros_status (NISYROS_CALL *instance_open)(
        const nisyros_plugin_services *services, void **out);
    void (NISYROS_CALL *instance_close)(void *instance);
    nisyros_status (NISYROS_CALL *device_count)(void *instance, uint32_t *out);
    nisyros_status (NISYROS_CALL *device_info)(
        void *instance, uint32_t index, nisyros_plugin_device_info *out);
    nisyros_status (NISYROS_CALL *queue_family_info)(
        void *instance, uint32_t device_index, uint32_t family,
        nisyros_plugin_queue_family_info *out);
    nisyros_status (NISYROS_CALL *memory_heap_info)(
        void *instance, uint32_t device_index, uint32_t heap,
        nisyros_plugin_memory_heap_info *out);
    nisyros_status (NISYROS_CALL *cooperative_matrix)(
        void *instance, uint32_t device_index, uint32_t tuple,
        nisyros_cooperative_matrix_tuple *out);
    nisyros_status (NISYROS_CALL *device_open)(
        void *instance, uint32_t index, uint32_t request_count,
        const nisyros_plugin_queue_request *requests, void **out);
    void (NISYROS_CALL *device_close)(void *device);
    nisyros_status (NISYROS_CALL *allocation_create)(
        void *device, const nisyros_plugin_alloc_request *request,
        void **out, nisyros_plugin_allocation_info *info);
    void (NISYROS_CALL *allocation_destroy)(void *device, void *allocation);
    nisyros_status (NISYROS_CALL *allocation_map)(
        void *device, void *allocation, uint64_t address, void **out);
    void (NISYROS_CALL *allocation_unmap)(void *device, void *allocation);
    nisyros_status (NISYROS_CALL *allocation_visibility)(
        void *device, void *allocation, uint32_t flush);
    nisyros_status (NISYROS_CALL *allocation_register_host)(
        void *device, void *host, uint64_t bytes, void **out,
        nisyros_plugin_allocation_info *info);
    nisyros_status (NISYROS_CALL *allocation_export_fd)(
        void *device, void *allocation, int32_t *out_fd);
    /* The import callback owns `fd` after the call, including on failure. */
    nisyros_status (NISYROS_CALL *allocation_import_fd)(
        void *device, void *source_allocation, uint64_t source_offset,
        uint64_t bytes, int32_t fd, void **out,
        nisyros_plugin_allocation_info *info);
    nisyros_status (NISYROS_CALL *device_wait_idle)(
        void *device, int32_t *backend_code);
    nisyros_status (NISYROS_CALL *timeline_create)(
        void *device, uint32_t exportable, void **out, int32_t *backend_code);
    /* The import callback owns `fd` after the call, including on failure. */
    nisyros_status (NISYROS_CALL *timeline_import_fd)(
        void *device, int32_t fd, void **out, int32_t *backend_code);
    void (NISYROS_CALL *timeline_destroy)(void *device, void *timeline);
    nisyros_status (NISYROS_CALL *timeline_query)(
        void *timeline, uint64_t *value, int32_t *backend_code);
    nisyros_status (NISYROS_CALL *timeline_wait)(
        void *timeline, uint64_t value, uint64_t timeout_ns,
        uint32_t *completed, int32_t *backend_code);
    nisyros_status (NISYROS_CALL *timeline_signal)(
        void *timeline, uint64_t value, int32_t *backend_code);
    nisyros_status (NISYROS_CALL *timeline_export_fd)(
        void *timeline, int32_t *out_fd, int32_t *backend_code);
    nisyros_status (NISYROS_CALL *pipeline_import)(
        void *device, const uint8_t *image, uint64_t image_bytes,
        void **out, int32_t *backend_code);
    void (NISYROS_CALL *pipeline_destroy)(void *device, void *pipeline);
    nisyros_status (NISYROS_CALL *query_create)(
        void *device, void **out, int32_t *backend_code);
    void (NISYROS_CALL *query_destroy)(void *device, void *query);
    nisyros_status (NISYROS_CALL *command_create)(
        void *device, uint32_t family, void **out, int32_t *backend_code);
    void (NISYROS_CALL *command_destroy)(void *device, void *command);
    nisyros_status (NISYROS_CALL *command_record)(
        void *command, uint32_t operation_count,
        const nisyros_plugin_command_op *operations, int32_t *backend_code);
    nisyros_status (NISYROS_CALL *queue_submit)(
        void *device, uint32_t queue_index, uint32_t segment_count,
        const nisyros_plugin_submit_segment *segments, int32_t *backend_code);
    nisyros_status (NISYROS_CALL *instance_native_handles)(
        void *instance, uint32_t device_index, void **native_instance,
        void **native_physical_device);
    nisyros_status (NISYROS_CALL *device_native_handles)(
        void *device, uint32_t queue_index, void **native_device,
        void **native_queue);
} nisyros_plugin_api;

/* All handles and callback storage belong to the provider that returned them. */
NISYROS_API nisyros_status NISYROS_CALL
nisyros_plugin_get_api(uint32_t abi_version,
                       const nisyros_plugin_services *services,
                       uint32_t table_size, void *table);

#ifdef __cplusplus
}
#endif

#endif
