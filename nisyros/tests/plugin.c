#include "nisyros_plugin.h"
#include <assert.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

static void report_error(void *context, nisyros_status status, int32_t code,
                         const char *message, uint32_t message_bytes) {
    (void)status;
    (void)code;
    (void)message;
    (void)message_bytes;
    ++*(int *)context;
}

int main(int argc, char **argv) {
    assert(argc == 2);
    assert(strcmp(argv[1], "strict") == 0 || strcmp(argv[1], "unsupported") == 0);
    int errors = 0;
    nisyros_plugin_services services = {
        sizeof(services), NISYROS_PLUGIN_SERVICES_VERSION, &errors, report_error,
    };
    nisyros_plugin_api api = {0};
    assert(nisyros_plugin_get_api(NISYROS_PLUGIN_ABI_VERSION, &services, 8,
                                  &api) == NISYROS_INCOMPLETE);
    assert(api.struct_size == sizeof(api) && api.instance_open == NULL);
    unsigned char oversized[sizeof(api) + 8];
    memset(oversized, 0xa5, sizeof(oversized));
    assert(nisyros_plugin_get_api(NISYROS_PLUGIN_ABI_VERSION, &services,
                                  sizeof(oversized), oversized) == NISYROS_SUCCESS);
    for (size_t i = sizeof(api); i < sizeof(oversized); ++i) assert(oversized[i] == 0xa5);
    assert(nisyros_plugin_get_api(NISYROS_PLUGIN_ABI_VERSION, &services,
                                  sizeof(api), &api) == NISYROS_SUCCESS);
    assert(api.instance_open && api.instance_close && api.device_count && api.device_info);
    assert(api.queue_family_info && api.memory_heap_info && api.cooperative_matrix);
    assert(api.device_open && api.device_close);
    assert(api.allocation_create && api.allocation_destroy && api.allocation_map);
    assert(api.allocation_unmap && api.allocation_visibility);
    assert(api.allocation_register_host && api.allocation_export_fd &&
           api.allocation_import_fd);
    assert(api.device_wait_idle && api.timeline_create && api.timeline_import_fd);
    assert(api.timeline_destroy && api.timeline_query && api.timeline_wait);
    assert(api.timeline_signal && api.timeline_export_fd);
    assert(api.pipeline_import && api.pipeline_destroy);
    assert(api.query_create && api.query_destroy);
    assert(api.command_create && api.command_destroy && api.command_record);
    assert(api.queue_submit);
    assert(sizeof(nisyros_plugin_command_op) == 72);
    nisyros_plugin_api unsupported = api;
    assert(nisyros_plugin_get_api(999, &services, sizeof(unsupported),
                                  &unsupported) == NISYROS_UNSUPPORTED);
    assert(unsupported.instance_open == NULL && unsupported.device_open == NULL);
    services.abi_version = 999;
    assert(nisyros_plugin_get_api(NISYROS_PLUGIN_ABI_VERSION, &services,
                                  sizeof(unsupported), &unsupported) == NISYROS_INVALID_ARGUMENT);
    assert(unsupported.instance_open == NULL);
    services.abi_version = NISYROS_PLUGIN_SERVICES_VERSION;

    void *instance = NULL;
    assert(api.instance_open(&services, &instance) == NISYROS_SUCCESS);
    assert(instance != NULL);
    uint32_t count = 0;
    assert(api.device_count(instance, &count) == NISYROS_SUCCESS);
    assert(count > 0 && count <= 32);
    nisyros_plugin_device_info info = {
        .struct_size = sizeof(info),
        .abi_version = NISYROS_PLUGIN_ABI_VERSION,
    };
    assert(api.device_info(instance, 0, &info) == NISYROS_SUCCESS);
    assert(info.backend == NISYROS_BACKEND_VULKAN);
    assert(info.name[0] && info.driver_name[0]);
    assert(info.queue_family_count > 0 && info.memory_heap_count > 0);
    assert((info.strict_profile_supported != 0) == (strcmp(argv[1], "strict") == 0));
    nisyros_plugin_memory_heap_info heap = {0};
    assert(api.memory_heap_info(instance, 0, 0, &heap) == NISYROS_SUCCESS);
    assert(heap.bytes > 0 && heap.reserved == 0);
    uint32_t family = UINT32_MAX;
    for (uint32_t i = 0; i < info.queue_family_count; ++i) {
        nisyros_plugin_queue_family_info queue = {0};
        assert(api.queue_family_info(instance, 0, i, &queue) == NISYROS_SUCCESS);
        if ((queue.flags & NISYROS_QUEUE_COMPUTE) && queue.count) family = i;
    }
    assert(family != UINT32_MAX);
    for (uint32_t i = 0; i < info.cooperative_matrix_count; ++i) {
        nisyros_cooperative_matrix_tuple tuple;
        assert(api.cooperative_matrix(instance, 0, i, &tuple) == NISYROS_SUCCESS);
        assert(tuple.m && tuple.n && tuple.k);
    }
    nisyros_plugin_queue_request request = {family, 1, 1.0f};
    void *device = NULL;
    nisyros_status opened = api.device_open(instance, 0, 1, &request, &device);
    if (info.strict_profile_supported) {
        assert(opened == NISYROS_SUCCESS && device != NULL);
        uint32_t family_index = family;
        nisyros_plugin_alloc_request allocation_request = {
            .bytes = 64,
            .alignment = 64,
            .required_properties = NISYROS_MEMORY_HOST_VISIBLE,
            .preferred_properties = NISYROS_MEMORY_HOST_COHERENT,
            .required_heap = UINT32_MAX,
            .preferred_heap = UINT32_MAX,
            .queue_family_count = 1,
            .queue_families = &family_index,
        };
        nisyros_plugin_allocation_info allocation_info = {0};
        void *allocation = NULL;
        assert(api.allocation_create(device, &allocation_request, &allocation,
                                     &allocation_info) == NISYROS_SUCCESS);
        assert(allocation != NULL && allocation_info.base != 0 && allocation_info.bytes == 64);
        assert(allocation_info.properties & NISYROS_MEMORY_HOST_VISIBLE);
        void *host = NULL;
        assert(api.allocation_map(device, allocation, allocation_info.base,
                                  &host) == NISYROS_SUCCESS);
        assert(host != NULL);
        memset(host, 0x5a, 64);
        assert(api.allocation_visibility(device, allocation, 1) == NISYROS_SUCCESS);
        assert(api.allocation_visibility(device, allocation, 0) == NISYROS_SUCCESS);
        api.allocation_unmap(device, allocation);
        api.allocation_destroy(device, allocation);
        void *timeline = NULL;
        int32_t backend_code = 0;
        assert(api.timeline_create(device, 0, &timeline, &backend_code) == NISYROS_SUCCESS);
        assert(timeline != NULL);
        uint64_t value = UINT64_MAX;
        assert(api.timeline_query(timeline, &value, &backend_code) == NISYROS_SUCCESS);
        assert(value == 0);
        uint32_t completed = 1;
        assert(api.timeline_wait(timeline, 1, 0, &completed, &backend_code) == NISYROS_SUCCESS);
        assert(completed == 0);
        assert(api.timeline_signal(timeline, 1, &backend_code) == NISYROS_SUCCESS);
        assert(api.timeline_wait(timeline, 1, 0, &completed, &backend_code) == NISYROS_SUCCESS);
        assert(completed == 1);
        void *query = NULL;
        assert(api.query_create(device, &query, &backend_code) == NISYROS_SUCCESS);
        assert(query != NULL);
        api.query_destroy(device, query);
        void *command = NULL;
        assert(api.command_create(device, family, &command, &backend_code) == NISYROS_SUCCESS);
        assert(command != NULL);
        assert(api.command_record(command, 0, NULL, &backend_code) == NISYROS_SUCCESS);
        nisyros_plugin_submit_point signal = {timeline, 2};
        nisyros_plugin_submit_segment segment = {command, 0, NULL, 1, &signal};
        assert(api.queue_submit(device, 0, 1, &segment, &backend_code) == NISYROS_SUCCESS);
        assert(api.timeline_wait(timeline, 2, 1000000000, &completed,
                                 &backend_code) == NISYROS_SUCCESS);
        assert(completed == 1);
        api.command_destroy(device, command);
        assert(api.device_wait_idle(device, &backend_code) == NISYROS_SUCCESS);
        api.timeline_destroy(device, timeline);
        api.device_close(device);
    } else {
        assert(opened == NISYROS_UNSUPPORTED && device == NULL);
    }
    api.instance_close(instance);
    assert(errors == 0);
    puts("linked provider table negotiated discovery and device opening");
    return 0;
}
