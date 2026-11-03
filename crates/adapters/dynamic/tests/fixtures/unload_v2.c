/* ABI v2 unload-safe fixture（ADR-0030）。
 * 只用于 conformance：不建线程、不用 TLS、不保留全局状态；
 * 所有返回字符串都由 free_string 释放。 */
#include <stdint.h>
#include <stddef.h>
#include <stdlib.h>
#include <string.h>

typedef void* (*create_fn)(const char*, const void*);
typedef char* (*invoke_fn)(void*, const char*);
typedef void (*destroy_fn)(void*);
typedef char* (*manifest_fn)(void);
typedef void (*free_string_fn)(char*);

typedef struct {
    uint32_t abi_version;
    uint32_t struct_size;
    uint32_t flags;
    manifest_fn manifest;
    create_fn create;
    invoke_fn invoke;
    destroy_fn destroy;
    free_string_fn free_string;
} acb_plugin_v2;

#ifndef PLUGIN_FLAGS
#define PLUGIN_FLAGS 1
#endif

static char* test_manifest(void) {
    const char* text =
        "{\"id\":\"test.unload\",\"platform\":\"test\",\"extension\":\"wake\","
        "\"api_version\":1,\"metadata\":{\"summary\":\"v2 fixture\","
        "\"effects\":[\"may_wake\"]}}";
    char* out = (char*)malloc(strlen(text) + 1);
    if (out != NULL) {
        strcpy(out, text);
    }
    return out;
}

static void* test_create(const char* config, const void* host) {
    (void)config;
    (void)host;
    return malloc(1);
}

static char* test_invoke(void* handle, const char* request) {
    (void)handle;
    (void)request;
    const char* text = "{\"ok\":true}";
    char* out = (char*)malloc(strlen(text) + 1);
    if (out != NULL) {
        strcpy(out, text);
    }
    return out;
}

static void test_destroy(void* handle) {
    free(handle);
}

static void test_free_string(char* text) {
    free(text);
}

static const acb_plugin_v2 TEST_TABLE = {
    .abi_version = 2,
    .struct_size = sizeof(acb_plugin_v2),
    .flags = PLUGIN_FLAGS,
    .manifest = test_manifest,
    .create = test_create,
    .invoke = test_invoke,
    .destroy = test_destroy,
    .free_string = test_free_string,
};

const acb_plugin_v2* acb_plugin_entry_v2(void) {
    return &TEST_TABLE;
}
