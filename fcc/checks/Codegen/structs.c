// RUN: fcc compile --stage ir -o - %S/../Inputs/structs.c | filecheck %s

// CHECK: cir.define_struct {sym_name = "Pair", fields = [{name = "tag", offset = 0, type = !i8}, {name = "value", offset = 4, type = !i32}], size = 8, align = 4}
// CHECK: func.func @read(%{{[0-9]+}}: !ptr.p) -> !i32 {
// CHECK: cir.get_member %{{[0-9]+}} {field = 1, struct_name = "Pair"} : !ptr.p
// CHECK: func.func @copy() -> !i32 {
// CHECK: ptr.alloca {size = 8, align = 4} : !ptr.p
// CHECK: cir.copy_struct
