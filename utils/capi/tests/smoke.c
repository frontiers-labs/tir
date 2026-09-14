/* C smoke test for the TIR C ABI: parse -> promote slots -> print.
 * Build/run via `xtask capi-smoke` (links against the cdylib). */
#include <stdio.h>
#include <string.h>

#include "tir.h"

static const char *MODULE =
    "module {\n"
    "  %fn_f = func.func @f(%0: !i32, %1: !i32) -> !i32 {\n"
    "    %2 = ptr.alloca {size = 4, align = 4} : !ptr.p<!i32>\n"
    "    ptr.store %0, %2\n"
    "    %5 = ptr.load %2 : !i32\n"
    "    %7 = muli %5, %5 : !i32\n"
    "    func.return %7\n"
    "  }\n"
    "  module_end\n"
    "}\n";

static const char *ROUND_MODULE =
    "#contract = {accuracy = {kind = \"reference\"}, arithmetic = {exceptions = \"ignore\", kind = \"arithmetic\", nan = \"any_quiet\", rounding = \"nearest_even\", subnormals = \"gradual\", tininess = \"after_rounding\"}, assumptions = {finite = false}, environment_epoch = 0, intermediate_exceptions = \"allow_contracted_group_removal\", permissions = {approved_approximation = false, cross_statement_contraction = false, expression_contraction = true, ignore_signed_zero = false, reassociation = false, reciprocal = false}, result_formats = [!f64]}\n"
    "module {\n"
    "  %a = fp.constant {bits = 0} : !f64\n"
    "  %r = fp.round (%x = %a) {contract = #contract} : !f64 {\n"
    "    -> %x\n"
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

    TirPassManager *pm =
        tir_pipeline_parse("func.func(restructure-nodes,promote-nodes,instcombine-nodes)");
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
        fprintf(stderr, "promotion did not remove allocas:\n%s\n", rendered);
        tir_string_free(rendered);
        return 1;
    }

    printf("%s\n", rendered);
    tir_string_free(rendered);

    uint32_t round_module = tir_parse_module(ctx, ROUND_MODULE, strlen(ROUND_MODULE));
    if (round_module == TIR_INVALID_ID) {
        fprintf(stderr, "round parse failed: %s\n", tir_last_error());
        return 1;
    }
    uint32_t module_block = tir_region_block(ctx, tir_op_region(ctx, round_module, 0), 0);
    uint32_t round = TIR_INVALID_ID;
    for (uintptr_t i = 0; i < tir_block_num_ops(ctx, module_block); ++i) {
        uint32_t op = tir_block_op(ctx, module_block, i);
        char *name = tir_op_name(ctx, op);
        char *dialect = tir_op_dialect(ctx, op);
        if (name && dialect && strcmp(name, "round") == 0 &&
            strcmp(dialect, "fp") == 0) {
            round = op;
        }
        tir_string_free(name);
        tir_string_free(dialect);
    }
    if (round == TIR_INVALID_ID) {
        fprintf(stderr, "fp.round not found\n");
        return 1;
    }
    bool saw_contract = false;
    for (uintptr_t i = 0; i < tir_op_num_attributes(ctx, round); ++i) {
        char *name = tir_op_attribute_name(ctx, round, i);
        if (name && strcmp(name, "contract") == 0) {
            saw_contract = tir_op_attribute_kind(ctx, round, i) == TIR_ATTR_DICT;
        }
        tir_string_free(name);
    }
    char *round_rendered = tir_op_to_string(ctx, round);
    if (!saw_contract || !round_rendered || strstr(round_rendered, "environment_epoch = 0") == NULL) {
        fprintf(stderr, "round contract inspection failed\n");
        tir_string_free(round_rendered);
        return 1;
    }
    tir_string_free(round_rendered);
    tir_context_destroy(ctx);
    return 0;
}
