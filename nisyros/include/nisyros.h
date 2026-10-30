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
typedef struct nisyros_event_token_impl *nisyros_event_token;
typedef struct nisyros_group_impl *nisyros_group;
typedef struct nisyros_endpoint_impl *nisyros_endpoint;
typedef struct nisyros_comm_cmd_impl *nisyros_comm_cmd;
typedef struct nisyros_executable_impl *nisyros_executable;
typedef struct nisyros_kernel_impl *nisyros_kernel;
typedef uint64_t nisyros_addr;

typedef struct {
    nisyros_device device;
    nisyros_addr address;
} nisyros_location;

#define NISYROS_TIMEOUT_INFINITE UINT64_MAX

typedef struct nisyros_extension {
    uint32_t struct_size;
    uint32_t type;
    uint32_t flags;
    uint32_t reserved;
    const struct nisyros_extension *next;
} nisyros_extension;

#define NISYROS_EXTENSION_OPTIONAL 1u
#define NISYROS_EXTENSION_ALLOC_HEAP 1u

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
#define NISYROS_BACKEND_ABI_VERSION 1u
#define NISYROS_VULKAN_INTEROP_ABI_VERSION 1u

#define NISYROS_FEATURE_COOPERATIVE_MATRIX UINT64_C(0x1)
#define NISYROS_FEATURE_BFLOAT16_TYPE UINT64_C(0x2)
#define NISYROS_FEATURE_BFLOAT16_DOT UINT64_C(0x4)
#define NISYROS_FEATURE_BFLOAT16_MATRIX UINT64_C(0x8)
#define NISYROS_FEATURE_SHADER_FLOAT16 UINT64_C(0x10)
#define NISYROS_FEATURE_VULKAN_MEMORY_MODEL UINT64_C(0x20)

#define NISYROS_COMPONENT_FLOAT16 0u
#define NISYROS_COMPONENT_FLOAT32 1u
#define NISYROS_COMPONENT_SINT8 3u
#define NISYROS_COMPONENT_SINT32 5u
#define NISYROS_COMPONENT_UINT8 7u
#define NISYROS_COMPONENT_UINT32 9u
#define NISYROS_COMPONENT_BFLOAT16 1000141000u
#define NISYROS_SCOPE_SUBGROUP 3u
#define NISYROS_STAGE_COMPUTE 0x20u

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
    uint32_t node_index;
    uint32_t is_local;
} nisyros_device_info;

typedef struct {
    nisyros_descriptor_header header;
    uint64_t features;
    uint32_t cooperative_matrix_revision;
    uint32_t shader_bfloat16_revision;
    uint32_t shader_enqueue_revision;
    uint32_t cooperative_matrix_stages;
} nisyros_device_capabilities;

typedef struct {
    uint32_t m;
    uint32_t n;
    uint32_t k;
    uint32_t a_type;
    uint32_t b_type;
    uint32_t c_type;
    uint32_t result_type;
    uint32_t saturating_accumulation;
    uint32_t scope;
} nisyros_cooperative_matrix_tuple;

typedef struct {
    uint32_t node_index;
    uint32_t reserved;
    const char *address;
} nisyros_node_address;

typedef struct {
    nisyros_descriptor_header header;
    uint64_t session_id;
    uint32_t local_node;
    uint32_t node_count;
    const nisyros_node_address *nodes;
    uint64_t connect_timeout_ns;
} nisyros_bootstrap_desc;

typedef struct {
    nisyros_descriptor_header header;
    uint64_t group_epoch;
    uint32_t member_count;
    const nisyros_device_id *members;
} nisyros_group_desc;

typedef struct {
    uint32_t rank;
    uint32_t reserved;
    nisyros_device device;
    nisyros_queue transfer_queue;
} nisyros_rank_binding;

typedef struct {
    nisyros_descriptor_header header;
    uint32_t binding_count;
    const nisyros_rank_binding *bindings;
    uint32_t allow_host_staging;
    uint32_t reserved;
    uint64_t failure_timeout_ns;
} nisyros_endpoint_desc;

typedef struct {
    nisyros_descriptor_header header;
    uint32_t software_progress;
    uint32_t worker_threads;
    uint32_t uses_host_staging;
    uint32_t pending_limit;
} nisyros_endpoint_info;

typedef struct {
    uint32_t slot;
    uint32_t reserved;
    uint64_t sequence;
} nisyros_sequence_binding;

typedef struct {
    nisyros_descriptor_header header;
    uint32_t binding_count;
    const nisyros_sequence_binding *bindings;
} nisyros_comm_submit_desc;

#define NISYROS_DTYPE_U32 1u
#define NISYROS_DTYPE_I32 2u
#define NISYROS_REDUCE_SUM 1u
#define NISYROS_REDUCE_MIN 2u
#define NISYROS_REDUCE_MAX 3u

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
    uint64_t bytes;
    uint32_t flags;
    uint32_t index;
} nisyros_memory_heap_info;

#define NISYROS_HEAP_DEVICE_LOCAL 0x00000001u

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
#define NISYROS_ACCESS_READ 0x00000001u
#define NISYROS_ACCESS_WRITE 0x00000002u

typedef struct {
    nisyros_descriptor_header header;
    uint64_t bytes;
    uint64_t alignment;
    uint32_t required_properties;
    uint32_t preferred_properties;
    uint32_t reserved[2];
} nisyros_alloc_desc;

typedef struct {
    nisyros_extension base;
    uint32_t required_heap;
    uint32_t preferred_heap;
    uint32_t reserved[2];
} nisyros_alloc_heap_extension;

typedef struct {
    nisyros_descriptor_header header;
    nisyros_addr base;
    uint64_t bytes;
    uint64_t backing_bytes;
    uint64_t heap_bytes;
    uint32_t memory_type_index;
    uint32_t heap_index;
    uint32_t properties;
    uint32_t mapped;
    uint32_t peer_imported;
    uint32_t access;
} nisyros_allocation_info;

typedef struct {
    nisyros_descriptor_header header;
    uint64_t bytes;
    uint64_t alignment;
    uint32_t reserved[2];
} nisyros_host_alloc_desc;

typedef struct {
    nisyros_descriptor_header header;
    uint32_t same_physical_device;
    uint32_t direct_mapping;
    uint32_t event_import;
    uint32_t host_staging;
} nisyros_peer_info;

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

typedef struct {
    nisyros_descriptor_header header;
    void *instance;
    void *physical_device;
    void *device;
} nisyros_vulkan_device_info;

typedef struct {
    nisyros_descriptor_header header;
    void *queue;
    uint32_t family;
    uint32_t index;
} nisyros_vulkan_queue_info;

typedef struct {
    uint32_t struct_size;
    uint32_t abi_version;
    nisyros_status (NISYROS_CALL *device_query)(nisyros_device,
                                                nisyros_vulkan_device_info *);
    nisyros_status (NISYROS_CALL *queue_query)(nisyros_queue,
                                               nisyros_vulkan_queue_info *);
} nisyros_vulkan_interop_api;

typedef struct {
    uint32_t struct_size;
    uint32_t abi_version;
    nisyros_status (NISYROS_CALL *get_extension)(
        nisyros_device, const char *, uint32_t, uint32_t, void *);
} nisyros_backend_api;

/* Tables are caller-owned. `struct_size` reports the provider's full size. */
NISYROS_API nisyros_status NISYROS_CALL
nisyros_backend_get_api(uint32_t backend, uint32_t abi_version,
                        uint32_t table_size, void *table);

/* Count is input capacity and output total. IDs remain valid for this cluster epoch. */
NISYROS_API nisyros_status NISYROS_CALL
nisyros_cluster_open_local(nisyros_cluster *out);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_cluster_join(const nisyros_bootstrap_desc *bootstrap,
                     nisyros_cluster *out);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_cluster_devices(nisyros_cluster cluster, uint32_t *count,
                        nisyros_device_id *ids);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_cluster_device_info(nisyros_cluster cluster, nisyros_device_id id,
                            nisyros_device_info *out);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_cluster_device_capabilities(nisyros_cluster cluster, nisyros_device_id id,
                                    nisyros_device_capabilities *out);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_cluster_cooperative_matrices(nisyros_cluster cluster, nisyros_device_id id,
                                     uint32_t *count,
                                     nisyros_cooperative_matrix_tuple *out);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_cluster_queue_family_info(nisyros_cluster cluster, nisyros_device_id id,
                                  uint32_t family,
                                  nisyros_queue_family_info *out);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_cluster_memory_heaps(nisyros_cluster cluster, nisyros_device_id id,
                             uint32_t *count, nisyros_memory_heap_info *out);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_cluster_close(nisyros_cluster cluster);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_group_create(nisyros_cluster cluster, const nisyros_group_desc *desc,
                     nisyros_group *out);
NISYROS_API nisyros_status NISYROS_CALL nisyros_group_destroy(nisyros_group group);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_group_rank_count(nisyros_group group, uint32_t *count);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_group_member(nisyros_group group, uint32_t rank,
                     nisyros_device_id *out);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_group_epoch(nisyros_group group, uint64_t *epoch);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_endpoint_open(nisyros_group group, const nisyros_endpoint_desc *desc,
                      nisyros_endpoint *out);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_endpoint_query(nisyros_endpoint endpoint, nisyros_endpoint_info *out);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_endpoint_close(nisyros_endpoint endpoint);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_comm_cmd_create(nisyros_endpoint endpoint, nisyros_comm_cmd *out);
NISYROS_API nisyros_status NISYROS_CALL nisyros_comm_cmd_begin(nisyros_comm_cmd cmd);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_comm_cmd_wait(nisyros_comm_cmd cmd, nisyros_point point);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_comm_cmd_send(nisyros_comm_cmd cmd, uint32_t local_rank,
                      uint32_t peer_rank, uint64_t channel, uint64_t sequence,
                      nisyros_addr source, uint64_t bytes);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_comm_cmd_send_slot(nisyros_comm_cmd cmd, uint32_t local_rank,
                           uint32_t peer_rank, uint64_t channel, uint32_t slot,
                           nisyros_addr source, uint64_t bytes);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_comm_cmd_recv(nisyros_comm_cmd cmd, uint32_t local_rank,
                      uint32_t peer_rank, uint64_t channel, uint64_t sequence,
                      nisyros_addr destination, uint64_t bytes);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_comm_cmd_recv_slot(nisyros_comm_cmd cmd, uint32_t local_rank,
                           uint32_t peer_rank, uint64_t channel, uint32_t slot,
                           nisyros_addr destination, uint64_t bytes);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_comm_cmd_all_reduce(nisyros_comm_cmd cmd, uint32_t local_rank,
                            uint64_t sequence, nisyros_addr source,
                            nisyros_addr destination, uint64_t count,
                            uint32_t datatype, uint32_t operation);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_comm_cmd_all_reduce_slot(nisyros_comm_cmd cmd, uint32_t local_rank,
                                 uint32_t slot, nisyros_addr source,
                                 nisyros_addr destination, uint64_t count,
                                 uint32_t datatype, uint32_t operation);
NISYROS_API nisyros_status NISYROS_CALL nisyros_comm_cmd_end(nisyros_comm_cmd cmd);
NISYROS_API nisyros_status NISYROS_CALL nisyros_comm_cmd_reset(nisyros_comm_cmd cmd);
NISYROS_API nisyros_status NISYROS_CALL nisyros_comm_cmd_destroy(nisyros_comm_cmd cmd);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_endpoint_submit(nisyros_endpoint endpoint, nisyros_comm_cmd cmd,
                        const nisyros_comm_submit_desc *desc, nisyros_point *out);
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
nisyros_allocation_query(nisyros_device device, nisyros_addr base,
                         nisyros_allocation_info *out);
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
nisyros_peer_capabilities(nisyros_device device, nisyros_device peer,
                          nisyros_peer_info *out);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_peer_map(nisyros_device device, nisyros_location source,
                 uint64_t bytes, uint32_t access, nisyros_addr *out);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_peer_unmap(nisyros_device device, nisyros_addr base);
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
nisyros_event_export(nisyros_event event, nisyros_event_token *out);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_event_import(nisyros_device device, nisyros_event_token token,
                     nisyros_event *out);
NISYROS_API nisyros_status NISYROS_CALL
nisyros_event_token_destroy(nisyros_event_token token);
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
