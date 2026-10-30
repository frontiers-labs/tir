#include "nisyros.h"
#include <assert.h>
#include <dlfcn.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef struct {
    int32_t type;
    const void *next;
    uint32_t flags;
    uint32_t family;
} native_pool_info;
typedef struct {
    int32_t type;
    const void *next;
    uint64_t pool;
    int32_t level;
    uint32_t count;
} native_command_alloc;
typedef struct {
    int32_t type;
    const void *next;
    uint32_t flags;
    const void *inheritance;
} native_begin_info;
typedef struct { uint64_t address, size; } native_range;
typedef struct {
    int32_t type;
    const void *next;
    void *command;
    uint32_t device_mask;
} native_command_submit;
typedef struct {
    int32_t type;
    const void *next;
    uint32_t flags, wait_count;
    const void *waits;
    uint32_t command_count;
    const native_command_submit *commands;
    uint32_t signal_count;
    const void *signals;
} native_submit_info;

static void native_fill(nisyros_vulkan_device_info device,
                        nisyros_vulkan_queue_info queue, nisyros_addr address) {
    void *loader = dlopen("libvulkan.so.1", RTLD_NOW);
    assert(loader != NULL);
    void *(*get_proc)(void *, const char *) = dlsym(loader, "vkGetDeviceProcAddr");
    assert(get_proc != NULL);
    int (*create_pool)(void *, const native_pool_info *, const void *, uint64_t *) =
        get_proc(device.device, "vkCreateCommandPool");
    int (*allocate)(void *, const native_command_alloc *, void **) =
        get_proc(device.device, "vkAllocateCommandBuffers");
    int (*begin)(void *, const native_begin_info *) = get_proc(device.device, "vkBeginCommandBuffer");
    void (*fill)(void *, const native_range *, uint32_t, uint32_t) =
        get_proc(device.device, "vkCmdFillMemoryKHR");
    int (*end)(void *) = get_proc(device.device, "vkEndCommandBuffer");
    int (*submit)(void *, uint32_t, const native_submit_info *, uint64_t) =
        get_proc(device.device, "vkQueueSubmit2");
    int (*idle)(void *) = get_proc(device.device, "vkQueueWaitIdle");
    void (*destroy_pool)(void *, uint64_t, const void *) =
        get_proc(device.device, "vkDestroyCommandPool");
    assert(create_pool && allocate && begin && fill && end && submit && idle && destroy_pool);
    native_pool_info pool_info = {39, NULL, 0, queue.family};
    uint64_t pool = 0;
    assert(create_pool(device.device, &pool_info, NULL, &pool) == 0);
    native_command_alloc command_alloc = {40, NULL, pool, 0, 1};
    void *command = NULL;
    assert(allocate(device.device, &command_alloc, &command) == 0);
    native_begin_info begin_info = {42, NULL, 0, NULL};
    assert(begin(command, &begin_info) == 0);
    native_range range = {address, 4};
    fill(command, &range, 4, 0xa1b2c3d4);
    assert(end(command) == 0);
    native_command_submit command_submit = {1000314006, NULL, command, 0};
    native_submit_info submit_info = {1000314004, NULL, 0, 0, NULL, 1,
                                      &command_submit, 0, NULL};
    assert(submit(queue.queue, 1, &submit_info, 0) == 0);
    assert(idle(queue.queue) == 0);
    destroy_pool(device.device, pool, NULL);
    assert(dlclose(loader) == 0);
}

int main(int argc, char **argv) {
    assert(argc == 2);
    assert(strcmp(argv[1], "strict") == 0 || strcmp(argv[1], "unsupported") == 0 ||
           strcmp(argv[1], "auto") == 0);
    int expect_strict = strcmp(argv[1], "strict") == 0;
    nisyros_cluster cluster = NULL;
    assert(nisyros_cluster_open_local(&cluster) == NISYROS_SUCCESS);
    assert(cluster != NULL);
    nisyros_backend_api backend_api = {0};
    assert(nisyros_backend_get_api(NISYROS_BACKEND_VULKAN, NISYROS_BACKEND_ABI_VERSION,
                                    7, &backend_api) == NISYROS_INVALID_ARGUMENT);
    assert(nisyros_backend_get_api(NISYROS_BACKEND_VULKAN, NISYROS_BACKEND_ABI_VERSION,
                                    8, &backend_api) == NISYROS_INCOMPLETE);
    assert(backend_api.struct_size == sizeof(backend_api) && backend_api.get_extension == NULL);
    unsigned char oversized[sizeof(backend_api) + 8];
    memset(oversized, 0xa5, sizeof(oversized));
    assert(nisyros_backend_get_api(NISYROS_BACKEND_VULKAN, NISYROS_BACKEND_ABI_VERSION,
                                    sizeof(oversized), oversized) == NISYROS_SUCCESS);
    for (size_t i = sizeof(backend_api); i < sizeof(oversized); ++i) assert(oversized[i] == 0xa5);
    assert(nisyros_backend_get_api(NISYROS_BACKEND_VULKAN, NISYROS_BACKEND_ABI_VERSION,
                                    sizeof(backend_api), &backend_api) == NISYROS_SUCCESS);
    assert(backend_api.get_extension != NULL);
    nisyros_backend_api unknown_api = backend_api;
    assert(nisyros_backend_get_api(NISYROS_BACKEND_VULKAN, 999,
                                    sizeof(unknown_api), &unknown_api) == NISYROS_UNSUPPORTED);
    assert(unknown_api.get_extension == NULL);

    uint32_t count = 0;
    assert(nisyros_cluster_devices(cluster, &count, NULL) == NISYROS_SUCCESS);
    assert(count > 0 && count <= 16);
    nisyros_device_id ids[16];
    uint32_t capacity = 0;
    assert(nisyros_cluster_devices(cluster, &capacity, ids) == NISYROS_INCOMPLETE);
    assert(capacity == count);
    capacity = 16;
    assert(nisyros_cluster_devices(cluster, &capacity, ids) == NISYROS_SUCCESS);

    nisyros_device_info info = {
        .header = {sizeof(info), NISYROS_ABI_VERSION, NULL},
    };
    assert(nisyros_cluster_device_info(cluster, ids[0], &info) == NISYROS_SUCCESS);
    nisyros_extension unknown = {sizeof(unknown), 0xfeed, NISYROS_EXTENSION_OPTIONAL, 0, NULL};
    info.header.extensions = &unknown;
    assert(nisyros_cluster_device_info(cluster, ids[0], &info) == NISYROS_SUCCESS);
    unknown.flags = 0;
    assert(nisyros_cluster_device_info(cluster, ids[0], &info) == NISYROS_UNSUPPORTED);
    info.header.extensions = NULL;
    assert(info.backend == NISYROS_BACKEND_VULKAN);
    if (strcmp(argv[1], "strict") == 0) {
        assert(info.device_type != NISYROS_DEVICE_TYPE_CPU);
    }
    if (strcmp(argv[1], "auto") == 0) expect_strict = info.strict_profile_supported != 0;
    assert((info.strict_profile_supported != 0) == expect_strict);
    printf("%s driver=%s strict=%u families=%u\n", info.name, info.driver_name,
           info.strict_profile_supported, info.queue_family_count);
    nisyros_device_capabilities capabilities = {
        .header = {sizeof(capabilities), NISYROS_ABI_VERSION, NULL},
    };
    assert(nisyros_cluster_device_capabilities(cluster, ids[0], &capabilities) == NISYROS_SUCCESS);
    uint32_t tuple_count = 0;
    assert(nisyros_cluster_cooperative_matrices(cluster, ids[0], &tuple_count, NULL) == NISYROS_SUCCESS);
    assert(tuple_count <= 256);
    if (capabilities.features & NISYROS_FEATURE_COOPERATIVE_MATRIX) {
        assert(capabilities.cooperative_matrix_revision != 0);
        assert(capabilities.cooperative_matrix_stages & NISYROS_STAGE_COMPUTE);
        assert(tuple_count > 0);
        nisyros_cooperative_matrix_tuple tuples[256];
        uint32_t capacity = 256;
        assert(nisyros_cluster_cooperative_matrices(cluster, ids[0], &capacity, tuples) == NISYROS_SUCCESS);
        assert(capacity == tuple_count);
        for (uint32_t i = 0; i < tuple_count; ++i) {
            assert(tuples[i].m && tuples[i].n && tuples[i].k);
            assert(tuples[i].saturating_accumulation <= 1);
        }
        printf("cooperative matrices=%u first=%ux%ux%u types=%u,%u,%u,%u scope=%u\n",
               tuple_count, tuples[0].m, tuples[0].n, tuples[0].k,
               tuples[0].a_type, tuples[0].b_type, tuples[0].c_type,
               tuples[0].result_type, tuples[0].scope);
        if (getenv("NISYROS_PRINT_MATRICES")) {
            for (uint32_t i = 0; i < tuple_count; ++i) {
                printf("matrix %u: %ux%ux%u types=%u,%u,%u,%u saturation=%u scope=%u\n",
                       i, tuples[i].m, tuples[i].n, tuples[i].k, tuples[i].a_type,
                       tuples[i].b_type, tuples[i].c_type, tuples[i].result_type,
                       tuples[i].saturating_accumulation, tuples[i].scope);
            }
        }
    } else {
        assert(tuple_count == 0);
    }
    uint32_t heap_count = 0;
    assert(nisyros_cluster_memory_heaps(cluster, ids[0], &heap_count, NULL) == NISYROS_SUCCESS);
    assert(heap_count > 0 && heap_count <= 16);
    nisyros_memory_heap_info heaps[16];
    uint32_t heap_capacity = 0;
    assert(nisyros_cluster_memory_heaps(cluster, ids[0], &heap_capacity, heaps) == NISYROS_INCOMPLETE);
    assert(heap_capacity == heap_count);
    heap_capacity = 16;
    assert(nisyros_cluster_memory_heaps(cluster, ids[0], &heap_capacity, heaps) == NISYROS_SUCCESS);
    for (uint32_t index = 0; index < heap_count; ++index) {
        assert(heaps[index].index == index && heaps[index].bytes > 0);
    }

    uint32_t selected = UINT32_MAX;
    uint32_t queue_count = 0;
    for (uint32_t family = 0; family < info.queue_family_count; ++family) {
        nisyros_queue_family_info queue;
        assert(nisyros_cluster_queue_family_info(cluster, ids[0], family, &queue) ==
               NISYROS_SUCCESS);
        assert(queue.family == family);
        assert(queue.engine_identity_known == 0);
        assert(queue.affinity_enforceable == 0);
        if ((queue.flags & NISYROS_QUEUE_COMPUTE) != 0 && queue.count > queue_count) {
            selected = family;
            queue_count = queue.count;
        }
    }
    assert(selected != UINT32_MAX);

    nisyros_queue_request request = {selected, queue_count < 2 ? 1 : 2, 1.0f, 0};
    nisyros_device_open_desc desc = {
        .header = {sizeof(desc), NISYROS_ABI_VERSION, NULL},
        .queue_request_count = 1,
        .queue_requests = &request,
    };
    nisyros_device device = NULL;
    nisyros_status status = nisyros_device_open(cluster, ids[0], &desc, &device);
    if (expect_strict) {
        assert(status == NISYROS_SUCCESS);
        assert(device != NULL);
        nisyros_vulkan_interop_api vk_api = {0};
        assert(backend_api.get_extension(device, "nisyros.vulkan", NISYROS_VULKAN_INTEROP_ABI_VERSION,
                                          8, &vk_api) == NISYROS_INCOMPLETE);
        assert(vk_api.struct_size == sizeof(vk_api) && vk_api.device_query == NULL);
        assert(backend_api.get_extension(device, "nisyros.vulkan", NISYROS_VULKAN_INTEROP_ABI_VERSION,
                                          sizeof(vk_api), &vk_api) == NISYROS_SUCCESS);
        assert(vk_api.device_query != NULL && vk_api.queue_query != NULL);
        nisyros_vulkan_device_info native_device = {
            .header = {sizeof(native_device), NISYROS_ABI_VERSION, NULL},
        };
        assert(vk_api.device_query(device, &native_device) == NISYROS_SUCCESS);
        assert(native_device.instance != NULL && native_device.physical_device != NULL &&
               native_device.device != NULL);
        nisyros_vulkan_interop_api unknown_vk = vk_api;
        assert(backend_api.get_extension(device, "nisyros.unknown", NISYROS_VULKAN_INTEROP_ABI_VERSION,
                                          sizeof(unknown_vk), &unknown_vk) == NISYROS_UNSUPPORTED);
        assert(unknown_vk.device_query == NULL && unknown_vk.queue_query == NULL);
        unknown_vk = vk_api;
        assert(backend_api.get_extension(device, "nisyros.vulkan", 999,
                                          sizeof(unknown_vk), &unknown_vk) == NISYROS_UNSUPPORTED);
        assert(unknown_vk.device_query == NULL && unknown_vk.queue_query == NULL);
        assert(nisyros_cluster_close(cluster) == NISYROS_BUSY);
        for (uint32_t index = 0; index < request.count; ++index) {
            nisyros_queue queue = NULL;
            assert(nisyros_queue_get(device, selected, index, &queue) == NISYROS_SUCCESS);
            assert(queue != NULL);
            nisyros_vulkan_queue_info native_queue = {
                .header = {sizeof(native_queue), NISYROS_ABI_VERSION, NULL},
            };
            assert(vk_api.queue_query(queue, &native_queue) == NISYROS_SUCCESS);
            assert(native_queue.queue != NULL && native_queue.family == selected &&
                   native_queue.index == index);
        }
        nisyros_alloc_desc allocation = {
            .header = {sizeof(allocation), NISYROS_ABI_VERSION, NULL},
            .bytes = 64,
            .alignment = 256,
            .required_properties = NISYROS_MEMORY_HOST_VISIBLE,
            .preferred_properties = NISYROS_MEMORY_HOST_COHERENT,
        };
        nisyros_addr address = 0;
        assert(nisyros_alloc(device, &allocation, &address) == NISYROS_SUCCESS);
        assert(address != 0 && address % 256 == 0);
        nisyros_allocation_info allocation_info = {
            .header = {sizeof(allocation_info), NISYROS_ABI_VERSION, NULL},
        };
        assert(nisyros_allocation_query(device, address, &allocation_info) == NISYROS_SUCCESS);
        assert(allocation_info.base == address && allocation_info.bytes == 64);
        assert(allocation_info.backing_bytes >= allocation_info.bytes);
        assert(allocation_info.heap_bytes >= allocation_info.backing_bytes);
        assert((allocation_info.properties & NISYROS_MEMORY_HOST_VISIBLE) != 0);
        assert(allocation_info.mapped == 0 && allocation_info.peer_imported == 0);
        assert(allocation_info.access == (NISYROS_ACCESS_READ | NISYROS_ACCESS_WRITE));
        assert(nisyros_allocation_query(device, address + 4, &allocation_info) == NISYROS_INVALID_ARGUMENT);
        nisyros_alloc_heap_extension heap_choice = {
            .base = {sizeof(heap_choice), NISYROS_EXTENSION_ALLOC_HEAP, 0, 0, NULL},
            .required_heap = allocation_info.heap_index,
            .preferred_heap = UINT32_MAX,
        };
        allocation.header.extensions = &heap_choice.base;
        nisyros_addr selected_heap_address = 0;
        assert(nisyros_alloc(device, &allocation, &selected_heap_address) == NISYROS_SUCCESS);
        assert(nisyros_allocation_query(device, selected_heap_address, &allocation_info) == NISYROS_SUCCESS);
        assert(allocation_info.heap_index == heap_choice.required_heap);
        assert(nisyros_free(device, selected_heap_address) == NISYROS_SUCCESS);
        heap_choice.preferred_heap = heap_choice.required_heap;
        heap_choice.required_heap = UINT32_MAX;
        assert(nisyros_alloc(device, &allocation, &selected_heap_address) == NISYROS_SUCCESS);
        assert(nisyros_allocation_query(device, selected_heap_address, &allocation_info) == NISYROS_SUCCESS);
        assert(allocation_info.heap_index == heap_choice.preferred_heap);
        assert(nisyros_free(device, selected_heap_address) == NISYROS_SUCCESS);
        heap_choice.required_heap = heap_count;
        assert(nisyros_alloc(device, &allocation, &selected_heap_address) == NISYROS_UNSUPPORTED);
        assert(selected_heap_address == 0);
        allocation.header.extensions = NULL;
        assert(nisyros_device_close(device) == NISYROS_BUSY);
        void *host = NULL;
        assert(nisyros_map(device, address + 4, 4, &host) == NISYROS_SUCCESS);
        assert(host != NULL);
        assert(nisyros_allocation_query(device, address, &allocation_info) == NISYROS_SUCCESS);
        assert(allocation_info.mapped == 1);
        memset(host, 0xa5, 4);
        assert(nisyros_flush(device, address + 4, 4) == NISYROS_SUCCESS);
        assert(nisyros_invalidate(device, address + 4, 4) == NISYROS_SUCCESS);
        void *mapped = host;
        assert(nisyros_map(device, address + 63, 2, &host) == NISYROS_INVALID_ARGUMENT);
        assert(host == NULL);
        assert(nisyros_free(device, address) == NISYROS_BUSY);
        assert(nisyros_map(device, address + 4, 4, &host) == NISYROS_INVALID_ARGUMENT);
        assert(host == NULL);
        assert(nisyros_free(device, address + 4) == NISYROS_INVALID_ARGUMENT);
        assert(nisyros_unmap(device, (void *)(uintptr_t)1) == NISYROS_INVALID_ARGUMENT);
        assert(nisyros_unmap(device, mapped) == NISYROS_SUCCESS);
        assert(nisyros_allocation_query(device, address, &allocation_info) == NISYROS_SUCCESS);
        assert(allocation_info.mapped == 0);
        assert(nisyros_free(device, address) == NISYROS_SUCCESS);
        assert(nisyros_allocation_query(device, address, &allocation_info) == NISYROS_INVALID_ARGUMENT);

        nisyros_addr source = 0, destination = 0;
        assert(nisyros_alloc(device, &allocation, &source) == NISYROS_SUCCESS);
        assert(nisyros_alloc(device, &allocation, &destination) == NISYROS_SUCCESS);
        void *source_host = NULL, *destination_host = NULL;
        assert(nisyros_map(device, source, 64, &source_host) == NISYROS_SUCCESS);
        assert(nisyros_map(device, destination, 64, &destination_host) == NISYROS_SUCCESS);
        memset(source_host, 0x3c, 64);
        memset(destination_host, 0, 64);
        assert(nisyros_flush(device, source, 64) == NISYROS_SUCCESS);
        assert(nisyros_flush(device, destination, 64) == NISYROS_SUCCESS);
        nisyros_queue interop_queue = NULL;
        assert(nisyros_queue_get(device, selected, 0, &interop_queue) == NISYROS_SUCCESS);
        nisyros_vulkan_queue_info native_queue = {
            .header = {sizeof(native_queue), NISYROS_ABI_VERSION, NULL},
        };
        assert(vk_api.queue_query(interop_queue, &native_queue) == NISYROS_SUCCESS);
        native_fill(native_device, native_queue, source + 48);
        assert(nisyros_invalidate(device, source + 48, 4) == NISYROS_SUCCESS);
        assert(*(uint32_t *)((unsigned char *)source_host + 48) == 0xa1b2c3d4);
        *(uint32_t *)((unsigned char *)source_host + 48) = 0x3c3c3c3c;
        assert(nisyros_flush(device, source + 48, 4) == NISYROS_SUCCESS);

        nisyros_cmd cmd = NULL;
        assert(nisyros_cmd_create(device, selected, &cmd) == NISYROS_SUCCESS);
        assert(nisyros_cmd_begin(cmd) == NISYROS_SUCCESS);
        assert(nisyros_cmd_fill(cmd, destination + 1, 4, 0) == NISYROS_INVALID_ARGUMENT);
        assert(nisyros_cmd_end(cmd) == NISYROS_INVALID_ARGUMENT);
        assert(nisyros_cmd_reset(cmd) == NISYROS_SUCCESS);
        assert(nisyros_cmd_begin(cmd) == NISYROS_SUCCESS);
        assert(nisyros_cmd_copy(cmd, destination + 8, source + 4, 16) == NISYROS_SUCCESS);
        assert(nisyros_cmd_barrier_range(cmd, destination + 8, 16) == NISYROS_SUCCESS);
        assert(nisyros_cmd_barrier_all(cmd) == NISYROS_SUCCESS);
        assert(nisyros_cmd_fill(cmd, destination + 24, 8, 0x01020304) == NISYROS_SUCCESS);
        assert(nisyros_cmd_end(cmd) == NISYROS_SUCCESS);
        assert(nisyros_free(device, source) == NISYROS_BUSY);
        nisyros_queue queue = NULL;
        assert(nisyros_queue_get(device, selected, 0, &queue) == NISYROS_SUCCESS);
        for (uint64_t iteration = 1; iteration <= 2; ++iteration) {
            nisyros_point done = {0};
            assert(nisyros_submit(queue, cmd, NULL, &done) == NISYROS_SUCCESS);
            assert(done.event != NULL && done.value == iteration);
            nisyros_status poll = nisyros_wait(done, 0);
            assert(poll == NISYROS_SUCCESS || poll == NISYROS_TIMEOUT);
            assert(nisyros_wait(done, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);
            uint64_t completed = 0;
            assert(nisyros_event_query(done.event, &completed) == NISYROS_SUCCESS);
            assert(completed >= iteration);
        }
        assert(nisyros_invalidate(device, destination, 64) == NISYROS_SUCCESS);
        for (int i = 8; i < 24; ++i) assert(((unsigned char *)destination_host)[i] == 0x3c);
        uint32_t pattern[2] = {0x01020304, 0x01020304};
        assert(memcmp((unsigned char *)destination_host + 24, pattern, sizeof(pattern)) == 0);
        assert(nisyros_cmd_destroy(cmd) == NISYROS_SUCCESS);
        if (request.count >= 2) {
            nisyros_event_desc event_desc = {
                .header = {sizeof(event_desc), NISYROS_ABI_VERSION, NULL},
                .signaling_domain = NISYROS_SIGNAL_DEVICE,
            };
            nisyros_event ready = NULL;
            assert(nisyros_event_create(device, &event_desc, &ready) == NISYROS_SUCCESS);
            nisyros_point ready_one = {ready, 1};
            nisyros_cmd producer = NULL, consumer = NULL, unordered = NULL;
            assert(nisyros_cmd_create(device, selected, &producer) == NISYROS_SUCCESS);
            assert(nisyros_cmd_begin(producer) == NISYROS_SUCCESS);
            assert(nisyros_cmd_fill(producer, source + 32, 16, 0x55667788) == NISYROS_SUCCESS);
            assert(nisyros_cmd_signal(producer, ready_one) == NISYROS_SUCCESS);
            assert(nisyros_cmd_end(producer) == NISYROS_SUCCESS);
            assert(nisyros_cmd_create(device, selected, &consumer) == NISYROS_SUCCESS);
            assert(nisyros_cmd_begin(consumer) == NISYROS_SUCCESS);
            assert(nisyros_cmd_wait(consumer, ready_one) == NISYROS_SUCCESS);
            assert(nisyros_cmd_copy(consumer, destination + 32, source + 32, 16) == NISYROS_SUCCESS);
            assert(nisyros_cmd_end(consumer) == NISYROS_SUCCESS);
            nisyros_queue queue_two = NULL;
            assert(nisyros_queue_get(device, selected, 1, &queue_two) == NISYROS_SUCCESS);
            nisyros_point consumer_done = {0}, producer_done = {0};
            assert(nisyros_submit(queue_two, consumer, NULL, &consumer_done) == NISYROS_SUCCESS);
            assert(nisyros_submit(queue, producer, NULL, &producer_done) == NISYROS_SUCCESS);
            assert(nisyros_event_destroy(ready) == NISYROS_BUSY);

            assert(nisyros_cmd_create(device, selected, &unordered) == NISYROS_SUCCESS);
            assert(nisyros_cmd_begin(unordered) == NISYROS_SUCCESS);
            assert(nisyros_cmd_signal(unordered, (nisyros_point){ready, 2}) == NISYROS_SUCCESS);
            assert(nisyros_cmd_end(unordered) == NISYROS_SUCCESS);
            nisyros_point rejected = {0};
            assert(nisyros_submit(queue_two, unordered, NULL, &rejected) == NISYROS_INVALID_ARGUMENT);
            assert(rejected.event == NULL);
            assert(nisyros_cmd_destroy(unordered) == NISYROS_SUCCESS);

            assert(nisyros_wait(consumer_done, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);
            assert(nisyros_wait(producer_done, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);
            assert(nisyros_invalidate(device, destination + 32, 16) == NISYROS_SUCCESS);
            uint32_t cross_queue_pattern[4] = {0x55667788, 0x55667788, 0x55667788, 0x55667788};
            assert(memcmp((unsigned char *)destination_host + 32, cross_queue_pattern,
                          sizeof(cross_queue_pattern)) == 0);
            assert(nisyros_cmd_destroy(producer) == NISYROS_SUCCESS);
            assert(nisyros_cmd_destroy(consumer) == NISYROS_SUCCESS);
            assert(nisyros_event_destroy(ready) == NISYROS_SUCCESS);

            nisyros_event left = NULL, right = NULL;
            assert(nisyros_event_create(device, &event_desc, &left) == NISYROS_SUCCESS);
            assert(nisyros_event_create(device, &event_desc, &right) == NISYROS_SUCCESS);
            nisyros_cmd left_producer = NULL, right_producer = NULL;
            nisyros_cmd joined = NULL, fanout = NULL;
            assert(nisyros_cmd_create(device, selected, &left_producer) == NISYROS_SUCCESS);
            assert(nisyros_cmd_begin(left_producer) == NISYROS_SUCCESS);
            assert(nisyros_cmd_fill(left_producer, source, 16, 0x11111111) == NISYROS_SUCCESS);
            assert(nisyros_cmd_signal(left_producer, (nisyros_point){left, 1}) == NISYROS_SUCCESS);
            assert(nisyros_cmd_end(left_producer) == NISYROS_SUCCESS);
            assert(nisyros_cmd_create(device, selected, &right_producer) == NISYROS_SUCCESS);
            assert(nisyros_cmd_begin(right_producer) == NISYROS_SUCCESS);
            assert(nisyros_cmd_fill(right_producer, source + 16, 16, 0x22222222) == NISYROS_SUCCESS);
            assert(nisyros_cmd_signal(right_producer, (nisyros_point){right, 1}) == NISYROS_SUCCESS);
            assert(nisyros_cmd_end(right_producer) == NISYROS_SUCCESS);
            assert(nisyros_cmd_create(device, selected, &joined) == NISYROS_SUCCESS);
            assert(nisyros_cmd_begin(joined) == NISYROS_SUCCESS);
            assert(nisyros_cmd_wait(joined, (nisyros_point){left, 1}) == NISYROS_SUCCESS);
            assert(nisyros_cmd_wait(joined, (nisyros_point){right, 1}) == NISYROS_SUCCESS);
            assert(nisyros_cmd_copy(joined, destination, source, 32) == NISYROS_SUCCESS);
            assert(nisyros_cmd_end(joined) == NISYROS_SUCCESS);
            assert(nisyros_cmd_create(device, selected, &fanout) == NISYROS_SUCCESS);
            assert(nisyros_cmd_begin(fanout) == NISYROS_SUCCESS);
            assert(nisyros_cmd_wait(fanout, (nisyros_point){left, 1}) == NISYROS_SUCCESS);
            assert(nisyros_cmd_fill(fanout, destination + 56, 4, 0x33333333) == NISYROS_SUCCESS);
            assert(nisyros_cmd_end(fanout) == NISYROS_SUCCESS);
            nisyros_point ignored = {0}, joined_done = {0}, fanout_done = {0};
            assert(nisyros_submit(queue, left_producer, NULL, &ignored) == NISYROS_SUCCESS);
            assert(nisyros_submit(queue_two, right_producer, NULL, &ignored) == NISYROS_SUCCESS);
            assert(nisyros_submit(queue, joined, NULL, &joined_done) == NISYROS_SUCCESS);
            assert(nisyros_submit(queue_two, fanout, NULL, &fanout_done) == NISYROS_SUCCESS);
            nisyros_point both_done[2] = {joined_done, fanout_done};
            assert(nisyros_wait_many(2, both_done, NISYROS_WAIT_ALL,
                                     NISYROS_TIMEOUT_INFINITE, NULL) == NISYROS_SUCCESS);
            assert(nisyros_invalidate(device, destination, 64) == NISYROS_SUCCESS);
            for (int i = 0; i < 16; ++i) assert(((unsigned char *)destination_host)[i] == 0x11);
            for (int i = 16; i < 32; ++i) assert(((unsigned char *)destination_host)[i] == 0x22);
            assert(*(uint32_t *)((unsigned char *)destination_host + 56) == 0x33333333);
            assert(nisyros_cmd_destroy(left_producer) == NISYROS_SUCCESS);
            assert(nisyros_cmd_destroy(right_producer) == NISYROS_SUCCESS);
            assert(nisyros_cmd_destroy(joined) == NISYROS_SUCCESS);
            assert(nisyros_cmd_destroy(fanout) == NISYROS_SUCCESS);
            assert(nisyros_event_destroy(left) == NISYROS_SUCCESS);
            assert(nisyros_event_destroy(right) == NISYROS_SUCCESS);
        }
        nisyros_event_desc replay_event_desc = {
            .header = {sizeof(replay_event_desc), NISYROS_ABI_VERSION, NULL},
            .signaling_domain = NISYROS_SIGNAL_DEVICE,
        };
        nisyros_event input_ready = NULL, output_ready = NULL;
        assert(nisyros_event_create(device, &replay_event_desc, &input_ready) == NISYROS_SUCCESS);
        assert(nisyros_event_create(device, &replay_event_desc, &output_ready) == NISYROS_SUCCESS);
        nisyros_cmd prepare = NULL, replay = NULL;
        assert(nisyros_cmd_create(device, selected, &prepare) == NISYROS_SUCCESS);
        assert(nisyros_cmd_begin(prepare) == NISYROS_SUCCESS);
        assert(nisyros_cmd_signal(prepare, (nisyros_point){input_ready, 1}) == NISYROS_SUCCESS);
        assert(nisyros_cmd_end(prepare) == NISYROS_SUCCESS);
        nisyros_point prepare_done = {0};
        assert(nisyros_submit(queue, prepare, NULL, &prepare_done) == NISYROS_SUCCESS);
        assert(nisyros_wait(prepare_done, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);

        assert(nisyros_cmd_create(device, selected, &replay) == NISYROS_SUCCESS);
        assert(nisyros_cmd_begin(replay) == NISYROS_SUCCESS);
        assert(nisyros_cmd_wait_slot(replay, 0) == NISYROS_SUCCESS);
        assert(nisyros_cmd_fill(replay, destination + 48, 16, 0xdeadbeef) == NISYROS_SUCCESS);
        assert(nisyros_cmd_signal_slot(replay, 1) == NISYROS_SUCCESS);
        assert(nisyros_cmd_end(replay) == NISYROS_SUCCESS);
        nisyros_cmd_info replay_info = {
            .header = {sizeof(replay_info), NISYROS_ABI_VERSION, NULL},
        };
        assert(nisyros_cmd_query(replay, &replay_info) == NISYROS_SUCCESS);
        assert(replay_info.native_segment_count == 1);
        assert(replay_info.native_record_count == 1);
        assert(replay_info.requires_host_progress == 0);
        nisyros_point replay_done = {0};
        assert(nisyros_submit(queue, replay, NULL, &replay_done) == NISYROS_INVALID_ARGUMENT);
        assert(replay_done.event == NULL);
        for (uint64_t iteration = 1; iteration <= 2; ++iteration) {
            nisyros_event_binding bindings[2] = {
                {0, 0, {input_ready, 1}},
                {1, 0, {output_ready, iteration}},
            };
            nisyros_submit_desc submit_desc = {
                .header = {sizeof(submit_desc), NISYROS_ABI_VERSION, NULL},
                .binding_count = 2,
                .bindings = bindings,
            };
            assert(nisyros_submit(queue, replay, &submit_desc, &replay_done) == NISYROS_SUCCESS);
            assert(nisyros_wait(replay_done, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);
            assert(nisyros_wait((nisyros_point){output_ready, iteration}, 0) == NISYROS_SUCCESS);
        }
        assert(nisyros_cmd_query(replay, &replay_info) == NISYROS_SUCCESS);
        assert(replay_info.native_record_count == 1);
        assert(nisyros_invalidate(device, destination + 48, 16) == NISYROS_SUCCESS);
        uint32_t replay_pattern[4] = {0xdeadbeef, 0xdeadbeef, 0xdeadbeef, 0xdeadbeef};
        assert(memcmp((unsigned char *)destination_host + 48, replay_pattern,
                      sizeof(replay_pattern)) == 0);
        assert(nisyros_cmd_destroy(prepare) == NISYROS_SUCCESS);
        assert(nisyros_cmd_destroy(replay) == NISYROS_SUCCESS);
        assert(nisyros_event_destroy(input_ready) == NISYROS_SUCCESS);
        assert(nisyros_event_destroy(output_ready) == NISYROS_SUCCESS);

        nisyros_event_desc host_event_desc = {
            .header = {sizeof(host_event_desc), NISYROS_ABI_VERSION, NULL},
            .signaling_domain = NISYROS_SIGNAL_HOST,
        };
        nisyros_event host_event = NULL;
        assert(nisyros_event_create(device, &host_event_desc, &host_event) == NISYROS_SUCCESS);
        nisyros_point host_one = {host_event, 1};
        assert(nisyros_wait(host_one, 0) == NISYROS_TIMEOUT);
        assert(nisyros_event_host_signal(host_event, 1) == NISYROS_SUCCESS);
        assert(nisyros_event_host_signal(host_event, 1) == NISYROS_INVALID_ARGUMENT);
        assert(nisyros_wait(host_one, 0) == NISYROS_SUCCESS);
        nisyros_point two_points[2] = {{host_event, 1}, {host_event, 2}};
        uint32_t satisfied = UINT32_MAX;
        assert(nisyros_wait_many(2, two_points, NISYROS_WAIT_ANY, 0, &satisfied) == NISYROS_SUCCESS);
        assert(satisfied == 0);
        assert(nisyros_wait_many(2, two_points, NISYROS_WAIT_ALL, 0, NULL) == NISYROS_TIMEOUT);
        nisyros_cmd host_consumer = NULL;
        assert(nisyros_cmd_create(device, selected, &host_consumer) == NISYROS_SUCCESS);
        assert(nisyros_cmd_begin(host_consumer) == NISYROS_SUCCESS);
        assert(nisyros_cmd_wait(host_consumer, two_points[1]) == NISYROS_SUCCESS);
        assert(nisyros_cmd_fill(host_consumer, destination + 40, 4, 0x12345678) == NISYROS_SUCCESS);
        assert(nisyros_cmd_end(host_consumer) == NISYROS_SUCCESS);
        nisyros_point host_consumer_done = {0};
        assert(nisyros_submit(queue, host_consumer, NULL, &host_consumer_done) == NISYROS_SUCCESS);
        assert(nisyros_event_host_signal(host_event, 2) == NISYROS_SUCCESS);
        assert(nisyros_wait_many(2, two_points, NISYROS_WAIT_ALL,
                                 NISYROS_TIMEOUT_INFINITE, NULL) == NISYROS_SUCCESS);
        assert(nisyros_wait(host_consumer_done, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);
        assert(nisyros_invalidate(device, destination + 40, 4) == NISYROS_SUCCESS);
        assert(*(uint32_t *)((unsigned char *)destination_host + 40) == 0x12345678);
        assert(nisyros_cmd_destroy(host_consumer) == NISYROS_SUCCESS);
        assert(nisyros_event_destroy(host_event) == NISYROS_SUCCESS);

        nisyros_host_alloc_desc host_desc = {
            .header = {sizeof(host_desc), NISYROS_ABI_VERSION, NULL},
            .bytes = 64,
            .alignment = 64,
        };
        void *upload = NULL, *download = NULL;
        assert(nisyros_host_alloc(device, &host_desc, &upload) == NISYROS_SUCCESS);
        assert(nisyros_host_alloc(device, &host_desc, &download) == NISYROS_SUCCESS);
        memset(upload, 0x5a, 64);
        memset(download, 0, 64);
        nisyros_cmd transfer = NULL;
        assert(nisyros_cmd_create(device, selected, &transfer) == NISYROS_SUCCESS);
        assert(nisyros_cmd_begin(transfer) == NISYROS_SUCCESS);
        assert(nisyros_cmd_copy_h2d(transfer, source, (unsigned char *)upload + 4, 16) == NISYROS_SUCCESS);
        assert(nisyros_cmd_barrier_all(transfer) == NISYROS_SUCCESS);
        assert(nisyros_cmd_copy_d2h(transfer, (unsigned char *)download + 8, source, 16) == NISYROS_SUCCESS);
        assert(nisyros_cmd_end(transfer) == NISYROS_SUCCESS);
        assert(nisyros_host_free(device, upload) == NISYROS_BUSY);
        nisyros_point transfer_done = {0};
        assert(nisyros_submit(queue, transfer, NULL, &transfer_done) == NISYROS_SUCCESS);
        assert(nisyros_wait(transfer_done, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);
        for (int i = 8; i < 24; ++i) assert(((unsigned char *)download)[i] == 0x5a);
        assert(nisyros_cmd_destroy(transfer) == NISYROS_SUCCESS);
        assert(nisyros_host_free(device, upload) == NISYROS_SUCCESS);
        assert(nisyros_host_free(device, download) == NISYROS_SUCCESS);

        uint64_t registration_alignment = 0;
        if (nisyros_host_registration_alignment(device, &registration_alignment) == NISYROS_SUCCESS) {
            assert(registration_alignment >= sizeof(void *));
            assert((registration_alignment & (registration_alignment - 1)) == 0);
            void *registered_upload = aligned_alloc((size_t)registration_alignment,
                                                    (size_t)registration_alignment);
            void *registered_download = aligned_alloc((size_t)registration_alignment,
                                                      (size_t)registration_alignment);
            assert(registered_upload != NULL && registered_download != NULL);
            memset(registered_upload, 0x69, (size_t)registration_alignment);
            memset(registered_download, 0, (size_t)registration_alignment);
            assert(nisyros_host_register(device, registered_upload, registration_alignment) == NISYROS_SUCCESS);
            assert(nisyros_host_register(device, registered_download, registration_alignment) == NISYROS_SUCCESS);
            nisyros_cmd registered_transfer = NULL;
            assert(nisyros_cmd_create(device, selected, &registered_transfer) == NISYROS_SUCCESS);
            assert(nisyros_cmd_begin(registered_transfer) == NISYROS_SUCCESS);
            assert(nisyros_cmd_copy_h2d(registered_transfer, source, registered_upload, 16) == NISYROS_SUCCESS);
            assert(nisyros_cmd_barrier_all(registered_transfer) == NISYROS_SUCCESS);
            assert(nisyros_cmd_copy_d2h(registered_transfer, registered_download, source, 16) == NISYROS_SUCCESS);
            assert(nisyros_cmd_end(registered_transfer) == NISYROS_SUCCESS);
            assert(nisyros_host_unregister(device, registered_upload) == NISYROS_BUSY);
            nisyros_point registered_done = {0};
            assert(nisyros_submit(queue, registered_transfer, NULL, &registered_done) == NISYROS_SUCCESS);
            assert(nisyros_wait(registered_done, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);
            for (int i = 0; i < 16; ++i) assert(((unsigned char *)registered_download)[i] == 0x69);
            assert(nisyros_cmd_destroy(registered_transfer) == NISYROS_SUCCESS);
            assert(nisyros_host_unregister(device, registered_upload) == NISYROS_SUCCESS);
            assert(nisyros_host_unregister(device, registered_download) == NISYROS_SUCCESS);
            free(registered_upload);
            free(registered_download);
            printf("registered host range verified (alignment=%llu)\n",
                   (unsigned long long)registration_alignment);
        }
        assert(nisyros_unmap(device, source_host) == NISYROS_SUCCESS);
        assert(nisyros_unmap(device, destination_host) == NISYROS_SUCCESS);
        assert(nisyros_free(device, source) == NISYROS_SUCCESS);
        assert(nisyros_free(device, destination) == NISYROS_SUCCESS);
        assert(nisyros_device_close(device) == NISYROS_SUCCESS);
    } else {
        assert(status == NISYROS_UNSUPPORTED);
        assert(device == NULL);
    }
    assert(nisyros_cluster_close(cluster) == NISYROS_SUCCESS);
    return 0;
}
