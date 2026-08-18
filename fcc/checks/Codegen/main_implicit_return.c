// RUN: fcc compile --stage ir -o - %s | filecheck %s

int main(void) { }

// CHECK: func.func @main() -> !i32 {
// CHECK-NEXT: %[[SLOT:[0-9]+]] = ptr.alloca
// CHECK-NEXT: %[[ZERO:[0-9]+]] = constant {value = 0} : !i32
// CHECK-NEXT: ptr.store %[[ZERO]], %[[SLOT]]
