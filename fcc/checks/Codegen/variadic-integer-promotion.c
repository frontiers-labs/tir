// RUN: fcc compile --stage ir -o - %s | filecheck %s

// CHECK: %{{[0-9]+}} = func.declare @consume(!i32, !varargs) -> !i32
// CHECK: %{{[0-9]+}} = func.func @main() -> !i32 {
// CHECK: %[[VALUE:[0-9]+]], %{{[0-9]+}} = ptr.load %{{[0-9]+}} state(%{{[0-9]+}}) : !i8
// CHECK: %[[PROMOTED:[0-9]+]] = extsi %[[VALUE]] : !i32
// CHECK-NEXT: %[[FP:[0-9]+]] = state.entry_state : !state<fp.env>
// CHECK-NEXT: %{{[0-9]+}}, %{{[0-9]+}}, %{{[0-9]+}} = func.call %{{[0-9]+}}(%{{[0-9]+}}, %[[PROMOTED]] : !i32, !i32) -> !i32 state(%{{[0-9]+}}, %[[FP]])
int consume(int marker, ...);

int main(void) {
    signed char value = -1;
    return consume(0, value);
}
