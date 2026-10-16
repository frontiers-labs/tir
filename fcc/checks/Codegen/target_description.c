// RUN: fcc compile --march riscv64 --stage ir -o - %s | filecheck %s
// RUN: fcc compile --march arm64 --stage ir -o - %s | filecheck %s --check-prefix=ARM64
// RUN: fcc compile --march rv64imc --stage ir -o - %s | filecheck %s --check-prefix=FEATURES

// The emitted module records the ABI and the chip it was compiled for, so the
// IR no longer depends on the driver flags to be lowered correctly.

int id(int value) { return value; }

// CHECK: #layout_spec = #layout.spec<endianness = "little", stack_align = 128, cache_line = 64
// CHECK-SAME: !ptr.p = {size = 64, abi = 64}
// CHECK: #target_env = {arch = "riscv64"
// CHECK: module {layout.spec = #layout_spec, target_env = #target_env} {

// ARM64: #target_env = {arch = "arm64"
// ARM64: module {layout.spec = #layout_spec, target_env = #target_env} {

// The environment names the extensions the ISA string enabled, and only those.
// FEATURES: #target_env = {arch = "riscv64", features = [
// FEATURES-SAME: "rvm"
// FEATURES-SAME: "c"
// FEATURES-NOT: "rvv"
