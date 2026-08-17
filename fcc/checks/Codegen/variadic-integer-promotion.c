// RUN: fcc compile --stage ir -o - %s | filecheck %s

// CHECK: func.declare @consume(!i32, !cir.varargs) -> !i32
// CHECK: func.func @main() -> !i32 {
// CHECK: %[[VALUE:[0-9]+]] = ptr.load %{{[0-9]+}} : !i8
// CHECK: %[[PROMOTED:[0-9]+]] = extsi %[[VALUE]] : !i32
// CHECK: func.call @consume(%{{[0-9]+}}, %[[PROMOTED]] : !i32, !i32) -> !i32
int consume(int marker, ...);

int main(void) {
    signed char value = -1;
    return consume(0, value);
}
