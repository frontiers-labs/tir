// RUN: fcc compile --march x86_64 --mabi sysv --stage ir -o - %s | filecheck %s
// RUN: fcc compile --march x86_64 --mabi sysv --stage asm -o - %s | filecheck %s --check-prefix=ASM

struct Large {
    long values[3];
};

long consume_large(struct Large value, long tail) {
    return value.values[0] + value.values[1] + value.values[2] + tail;
}

long forward_large(struct Large value, long tail) {
    return consume_large(value, tail);
}

// CHECK: func.func @consume_large(%[[VALUE:[0-9]+]]: !tuple<!i64, !i64, !i64>, %{{[0-9]+}}: !i64) -> !i64 {
// CHECK-DAG: %{{[0-9]+}} = tuple_get %[[VALUE]] {index = 0} : !i64
// CHECK-DAG: %{{[0-9]+}} = tuple_get %[[VALUE]] {index = 1} : !i64
// CHECK-DAG: %{{[0-9]+}} = tuple_get %[[VALUE]] {index = 2} : !i64
// CHECK: func.func @forward_large(%{{[0-9]+}}: !tuple<!i64, !i64, !i64>, %{{[0-9]+}}: !i64) -> !i64 {
// CHECK: %[[GROUP:[0-9]+]] = make_tuple %{{[0-9]+}}, %{{[0-9]+}}, %{{[0-9]+}} : !tuple<!i64, !i64, !i64>
// CHECK: func.call @consume_large(%[[GROUP]], %{{[0-9]+}} : !tuple<!i64, !i64, !i64>, !i64) -> !i64

// ASM-LABEL: consume_large:
// ASM-DAG: mov {{.*}}, [rsp + 40]
// ASM-DAG: mov {{.*}}, [rsp + 48]
// ASM-DAG: mov {{.*}}, [rsp + 56]
// ASM: add {{.*}}, rdi
// ASM-LABEL: forward_large:
// ASM: add rsp, -72
// ASM: mov [rsp + 0], {{.*}}
// ASM: mov [rsp + 8], {{.*}}
// ASM: mov [rsp + 16], {{.*}}
// ASM: mov rdi, {{.*}}
// ASM: call consume_large
