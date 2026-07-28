// RUN: fcc compile --march riscv64 --stage ir -o - %s | filecheck %s
// RUN: fcc compile --march arm64 --stage ir -o - %s | filecheck %s --check-prefix=ARM64
// RUN: fcc compile --march rv64imc --stage ir -o - %s | filecheck %s --check-prefix=FEATURES

// The emitted module records the ABI and the chip it was compiled for, so the
// IR no longer depends on the driver flags to be lowered correctly.

int id(int value) { return value; }

// CHECK: #data_layout = {endianness = "little", stack_alignment = 128
// CHECK-SAME: p = {abi = 64, size = 64}
// CHECK: #target_env = {arch = "riscv64"
// CHECK: module {data_layout = #data_layout, target_env = #target_env} {

// ARM64: #target_env = {arch = "arm64"
// ARM64: module {data_layout = #data_layout, target_env = #target_env} {

// The environment names the extensions the ISA string enabled, and only those.
// FEATURES: #target_env = {arch = "riscv64", features = [
// FEATURES-SAME: "rvm"
// FEATURES-SAME: "c"
// FEATURES-NOT: "rvv"
