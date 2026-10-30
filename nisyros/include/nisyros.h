#ifndef NISYROS_H
#define NISYROS_H

#include <stdint.h>

#if defined(_WIN32)
#define NISYROS_API __declspec(dllimport)
#define NISYROS_CALL __cdecl
#else
#define NISYROS_API __attribute__((visibility("default")))
#define NISYROS_CALL
#endif

#ifdef __cplusplus
extern "C" {
#endif

#define NISYROS_ABI_VERSION 1u

typedef int32_t nisyros_status;
#define NISYROS_SUCCESS 0
#define NISYROS_INCOMPLETE 1
#define NISYROS_INVALID_ARGUMENT -1
#define NISYROS_UNSUPPORTED -2
#define NISYROS_NO_DEVICE -3
#define NISYROS_OUT_OF_MEMORY -4
#define NISYROS_BUSY -5
#define NISYROS_DEVICE_LOST -6
#define NISYROS_TIMEOUT -7
#define NISYROS_SUBMISSION_FAILED -8
#define NISYROS_INCOMPATIBLE_BINARY -9
#define NISYROS_PEER_LOST -10

typedef struct nisyros_cluster_impl *nisyros_cluster;
typedef struct nisyros_device_impl *nisyros_device;
typedef struct nisyros_queue_impl *nisyros_queue;
typedef struct nisyros_cmd_impl *nisyros_cmd;
typedef struct nisyros_event_impl *nisyros_event;
typedef struct nisyros_executable_impl *nisyros_executable;
typedef struct nisyros_kernel_impl *nisyros_kernel;
typedef uint64_t nisyros_addr;

#define NISYROS_TIMEOUT_INFINITE UINT64_MAX

typedef struct nisyros_extension {
    uint32_t struct_size;
    uint32_t type;
    uint32_t flags;
    uint32_t reserved;
    const struct nisyros_extension *next;
} nisyros_extension;

#define NISYROS_EXTENSION_OPTIONAL 1u

typedef struct {
    uint32_t struct_size;
    uint32_t abi_version;
    const nisyros_extension *extensions;
} nisyros_descriptor_header;

typedef struct {
    uint64_t epoch;
    uint32_t index;
    uint32_t backend;
} nisyros_device_id;

#define NISYROS_BACKEND_VULKAN 1u
#define NISYROS_DEVICE_TYPE_CPU 4u

typedef struct {
    nisyros_descriptor_header header;
    uint32_t backend;
    uint32_t api_version;
    uint32_t vendor_id;
    uint32_t device_id;
    uint32_t queue_family_count;
    uint32_t strict_profile_supported;
    char name[256];
    uint8_t device_uuid[16];
    uint8_t driver_uuid[16];
    uint32_t driver_id;
    uint32_t device_type;
    char driver_name[256];
} nisyros_device_info;

#define NISYROS_QUEUE_COMPUTE 0x00000002u
#define NISYROS_QUEUE_TRANSFER 0x00000004u

typedef struct {
    uint32_t family;
    uint32_t count;
    uint32_t flags;
    uint32_t engine_identity_known;
    uint32_t affinity_enforceable;
} nisyros_queue_family_info;

typedef struct {
    uint32_t family;
    uint32_t count;
    float priority;
    uint32_t reserved;
} nisyros_queue_request;

typedef struct {
    nisyros_descriptor_header header;
    uint32_t queue_request_count;
    const nisyros_queue_request *queue_requests;
} nisyros_device_open_desc;

#define NISYROS_MEMORY_DEVICE_LOCAL 0x00000001u
#define NISYROS_MEMORY_HOST_VISIBLE 0x00000002u
#define NISYROS_MEMORY_HOST_COHERENT 0x00000004u

typedef struct {
    nisyros_descriptor_header header;
    uint64_t bytes;
    uint64_t alignment;
    uint32_t required_properties;
    uint32_t preferred_properties;
    uint32_t reserved[2];
} nisyros_alloc_desc;

typedef struct {
    nisyros_descriptor_header header;
    uint64_t bytes;
    uint64_t alignment;
    uint32_t reserved[2];
} nisyros_host_alloc_desc;

typedef struct {
    nisyros_event event;
    uint64_t value;
} nisyros_point;

#define NISYROS_SIGNAL_DEVICE 1u
#define NISYROS_SIGNAL_HOST 2u
#define NISYROS_WAIT_ALL 0u
#define NISYROS_WAIT_ANY 1u

typedef struct {
    nisyros_descriptor_header header;
    uint32_t signaling_domain;
    uint32_t reserved;
} nisyros_event_desc;

typedef struct {
    uint32_t slot;
    uint32_t reserved;
    nisyros_point point;
} nisyros_event_binding;

typedef struct {
    nisyros_descriptor_header header;
    uint32_t binding_count;
    const nisyros_event_binding *bindings;
} nisyros_submit_desc;

typedef struct {
    nisyros_descriptor_header header;
    uint32_t native_segment_count;
    uint32_t requires_host_progress;
    uint64_t native_record_count;
} nisyros_cmd_info;

#define NISYROS_IMAGE_VULKAN_PIPELINE_BINARY 1u

typedef struct {
    nisyros_descriptor_header header;
    uint32_t format;
    uint32_t reserved;
    uint64_t byte_size;
    const void *bytes;
} nisyros_image;

typedef struct {
    uint32_t x;
    uint32_t y;
    uint32_t z;
} nisyros_grid;

/* Count is input capacity and output total. IDs remain valid for this cluster epoch. */
NISYROS_API nisyros_status NISYROS_CALL
nisyros_cluster_open_local(nisyros_cluster *out);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_cluster_devices(nisyros_cluster cluster, uint32_t *count,
                        nisyros_device_id *ids);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_cluster_device_info(nisyros_cluster cluster, nisyros_device_id id,
                            nisyros_device_info *out);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_cluster_queue_family_info(nisyros_cluster cluster, nisyros_device_id id,
                                  uint32_t family,
                                  nisyros_queue_family_info *out);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_cluster_close(nisyros_cluster cluster);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_device_open(nisyros_cluster cluster, nisyros_device_id id,
                    const nisyros_device_open_desc *desc, nisyros_device *out);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_queue_get(nisyros_device device, uint32_t family, uint32_t index,
                  nisyros_queue *out);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_device_close(nisyros_device device);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_alloc(nisyros_device device, const nisyros_alloc_desc *desc,
              nisyros_addr *out);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_free(nisyros_device device, nisyros_addr base);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_map(nisyros_device device, nisyros_addr address, uint64_t bytes,
            void **host);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_unmap(nisyros_device device, void *host);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_flush(nisyros_device device, nisyros_addr address, uint64_t bytes);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_invalidate(nisyros_device device, nisyros_addr address, uint64_t bytes);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_host_alloc(nisyros_device device, const nisyros_host_alloc_desc *desc,
                   void **out);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_host_free(nisyros_device device, void *host);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_host_registration_alignment(nisyros_device device, uint64_t *alignment);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_host_register(nisyros_device device, void *host, uint64_t bytes);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_host_unregister(nisyros_device device, void *host);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_executable_load(nisyros_device device, const nisyros_image *image,
                        nisyros_executable *out);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_kernel_lookup(nisyros_executable executable, const char *entry,
                      nisyros_kernel *out);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_executable_destroy(nisyros_executable executable);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_cmd_create(nisyros_device device, uint32_t family, nisyros_cmd *out);
NISYROS_API nisyros_status NISYROS_CALL nisyros_cmd_begin(nisyros_cmd cmd);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_cmd_copy(nisyros_cmd cmd, nisyros_addr dst, nisyros_addr src,
                 uint64_t bytes);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_cmd_copy_h2d(nisyros_cmd cmd, nisyros_addr dst, const void *src,
                     uint64_t bytes);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_cmd_copy_d2h(nisyros_cmd cmd, void *dst, nisyros_addr src,
                     uint64_t bytes);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_cmd_fill(nisyros_cmd cmd, nisyros_addr dst, uint64_t bytes,
                 uint32_t pattern);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_cmd_dispatch_args(nisyros_cmd cmd, nisyros_kernel kernel,
                          const nisyros_grid *grid, nisyros_addr arguments);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_cmd_dispatch(nisyros_cmd cmd, nisyros_kernel kernel,
                     const nisyros_grid *grid, const void *arguments,
                     uint64_t argument_bytes);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_cmd_dispatch_indirect_args(nisyros_cmd cmd, nisyros_kernel kernel,
                                   nisyros_addr grid_address,
                                   nisyros_addr arguments);
NISYROS_API nisyros_status NISYROS_CALL nisyros_cmd_barrier_all(nisyros_cmd cmd);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_cmd_barrier_range(nisyros_cmd cmd, nisyros_addr address,
                          uint64_t bytes);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_event_create(nisyros_device device, const nisyros_event_desc *desc,
                     nisyros_event *out);
NISYROS_API nisyros_status NISYROS_CALL nisyros_event_destroy(nisyros_event event);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_event_host_signal(nisyros_event event, uint64_t value);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_cmd_wait(nisyros_cmd cmd, nisyros_point point);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_cmd_signal(nisyros_cmd cmd, nisyros_point point);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_cmd_wait_slot(nisyros_cmd cmd, uint32_t slot);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_cmd_signal_slot(nisyros_cmd cmd, uint32_t slot);
NISYROS_API nisyros_status NISYROS_CALL nisyros_cmd_end(nisyros_cmd cmd);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_cmd_query(nisyros_cmd cmd, nisyros_cmd_info *out);
NISYROS_API nisyros_status NISYROS_CALL nisyros_cmd_reset(nisyros_cmd cmd);
NISYROS_API nisyros_status NISYROS_CALL nisyros_cmd_destroy(nisyros_cmd cmd);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_submit(nisyros_queue queue, nisyros_cmd cmd,
               const nisyros_submit_desc *submit_desc,
               nisyros_point *out);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_wait(nisyros_point point, uint64_t timeout_ns);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_wait_many(uint32_t count, const nisyros_point *points, uint32_t mode,
                  uint64_t timeout_ns, uint32_t *satisfied_index);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_event_query(nisyros_event event, uint64_t *value);

#ifdef __cplusplus
}
#endif

#endif
