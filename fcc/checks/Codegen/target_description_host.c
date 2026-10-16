// RUN: fcc compile --stage ir -o - %s | filecheck %s

// Without --march the module still describes the host it was compiled for, so
// every module fcc emits carries its layout rather than only the flagged ones.

int id(int value) { return value; }

// CHECK: #layout_spec = #layout.spec<endianness =
// CHECK: #target_env = {arch =
// CHECK: module {layout.spec = #layout_spec, target_env = #target_env} {
