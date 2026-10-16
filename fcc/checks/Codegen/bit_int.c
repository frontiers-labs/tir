// RUN: fcc compile --std c23 --march x86_64 --stage ir -o - %s | filecheck %s
// RUN: fcc compile --std c23 --march aarch64 --stage ir -o - %s | filecheck %s --check-prefix=ARM64

// `_BitInt(N)` is the IR integer of width N. Its size and alignment come from
// the target's `_BitInt` rule in the module's layout spec: the smallest
// standard integer up to 64 bits, whole chunks above, 8 bytes wide on x86-64
// and 16 on AArch64. A `wb` literal is the narrowest `_BitInt` holding it,
// and one wider than 64 bits keeps every limb.

// CHECK: !builtin.i<W> = {size = !nat.align_to<W, !nat.min<!nat.max<!nat.pow2_ceil<W>, 8>, 64>>, abi = !nat.min<!nat.max<!nat.pow2_ceil<W>, 8>, 64>}
// ARM64: !builtin.i<W> = {size = !nat.align_to<W, !nat.min<!nat.max<!nat.pow2_ceil<W>, 8>, 128>>, abi = !nat.min<!nat.max<!nat.pow2_ceil<W>, 8>, 128>}

// A constant initializer extends as its own type does, and a literal wider
// than 64 bits keeps every limb.
unsigned _BitInt(65) one = 1;
_BitInt(100) minus_three = -3;
unsigned _BitInt(70) max_ull = 18446744073709551615ull;
unsigned _BitInt(128) two_to_64 = 18446744073709551616uwb;
// CHECK: global @one align 8 bytes [1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
// CHECK: global @minus_three align 8 bytes [253, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255]
// CHECK: global @max_ull align 8 bytes [255, 255, 255, 255, 255, 255, 255, 255, 0, 0, 0, 0, 0, 0, 0, 0]
// CHECK: global @two_to_64 align 8 bytes [0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0]

int max_width(void) { return __BITINT_MAXWIDTH__; }
// CHECK-LABEL: func.func @max_width
// CHECK: constant {value = 65535} : !i32

unsigned _BitInt(24) add24(unsigned _BitInt(24) a, unsigned _BitInt(24) b) { return a + b; }
// CHECK-LABEL: func.func @add24
// CHECK: addi %{{[0-9]+}}, %{{[0-9]+}} : !i24

int sizes(void) { return sizeof(_BitInt(24)) * 100 + sizeof(unsigned _BitInt(65)); }
// CHECK-LABEL: func.func @sizes
// CHECK: constant {value = 416} : !i64
// ARM64-LABEL: func.func @sizes
// ARM64: constant {value = 416} : !i64

_BitInt(200) big(_BitInt(200) a) { return a * 3wb + 0x123456789abcdef0123456789uwb; }
// CHECK-LABEL: func.func @big
// CHECK: ptr.alloca {size = 32, align = 8}
// CHECK: constant {value = 3} : !i3
// CHECK: constant {value = 90144042682896311822508713865} : !i97
// CHECK: muli %{{[0-9]+}}, %{{[0-9]+}} : !i200
// ARM64-LABEL: func.func @big
// ARM64: ptr.alloca {size = 32, align = 16}
