// RUN: fcc compile --stage ir -o - %s | filecheck %s

// CHECK: %{{[0-9]+}} = func.declare @consume(!i32, !cir.varargs) -> !i32
// CHECK: %{{[0-9]+}} = func.func @main() -> !i32 {
// CHECK: %[[VALUE:[0-9]+]] | %{{[0-9]+}} = ptr.load %{{[0-9]+}} | %{{[0-9]+}} : !i8
// CHECK: %[[PROMOTED:[0-9]+]] = extsi %[[VALUE]] : !i32
// CHECK: func.call %{{[0-9]+}}(%{{[0-9]+}}, %[[PROMOTED]] : !i32, !i32) -> !i32 | %{{[0-9]+}}
int consume(int marker, ...);

int main(void) {
    signed char value = -1;
    return consume(0, value);
}
