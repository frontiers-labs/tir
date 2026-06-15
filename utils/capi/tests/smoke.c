/* C smoke test for the TIR C ABI: parse -> run mem2reg -> print.
 * Build/run via `xtask capi-smoke` (links against the cdylib). */
#include <stdio.h>
#include <string.h>

#include "tir.h"

static const char *MODULE =
    "module {\n"
    "  func @f(%0: !i32, %1: !i32) -> !i32 {\n"
    "    %2 = ptr.alloca : !ptr.p<!i32>\n"
    "    ptr.store %0, %2\n"
    "    %5 = ptr.load %2 : !i32\n"
    "    %7 = muli %5, %5 : !i32\n"
    "    return %7\n"
    "  }\n"
    "  module_end\n"
    "}\n";

int main(void) {
    TirContext *ctx = tir_context_create();
    if (!ctx) {
        fprintf(stderr, "context create failed\n");
        return 1;
    }

    uint32_t module = tir_parse_module(ctx, MODULE, strlen(MODULE));
    if (module == TIR_INVALID_ID) {
        fprintf(stderr, "parse failed: %s\n", tir_last_error());
        return 1;
    }

    TirPassManager *pm = tir_pipeline_parse("builtin.func(mem2reg)");
    if (!pm) {
        fprintf(stderr, "pipeline parse failed: %s\n", tir_last_error());
        return 1;
    }
    if (!tir_pipeline_run(pm, ctx, module)) {
        fprintf(stderr, "pipeline run failed: %s\n", tir_last_error());
        return 1;
    }
    tir_pipeline_destroy(pm);

    char *rendered = tir_op_to_string(ctx, module);
    if (!rendered) {
        fprintf(stderr, "print failed: %s\n", tir_last_error());
        return 1;
    }
    if (strstr(rendered, "ptr.alloca") != NULL) {
        fprintf(stderr, "mem2reg did not remove allocas:\n%s\n", rendered);
        tir_string_free(rendered);
        return 1;
    }

    printf("%s\n", rendered);
    tir_string_free(rendered);
    tir_context_destroy(ctx);
    return 0;
}
