// RUN: fcc compile --stage ir -o - %s | filecheck %s

int main(void) { }

// CHECK: %{{[0-9]+}} = func.func @main() -> !i32 {
// CHECK-NEXT: %[[SLOT:[0-9]+]] = ptr.alloca
// CHECK-NEXT: %[[ZERO:[0-9]+]] = constant {value = 0} : !i32
// CHECK-NEXT: | %[[ENTRY:[0-9]+]] = state.entry_state
// CHECK-NEXT: | %{{[0-9]+}} = ptr.store %[[ZERO]], %[[SLOT]] | %[[ENTRY]]
