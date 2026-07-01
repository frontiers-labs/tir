# Object Emission

TIR emits ELF relocatable objects (`ET_REL`) directly, without an external
assembler. The pieces:

- **Instruction encoders** are generated from TMDL `encoding` blocks, next to
  the asm printers/parsers. An encoder returns the instruction bytes plus
  *fixups* for operands whose value is unknown at encode time: a basic-block
  target (`AttributeValue::Block`) or a symbol name (`AttributeValue::Str`).
  A *patcher* (also generated) re-scatters a resolved value into the
  operand's encoding bits, including non-contiguous immediates such as
  RISC-V B/J-type offsets.
- **`BinaryWriter`** (`tir::backend::binary`) walks lowered machine IR the
  same way the assembly printer does, lays out `.text`, records symbol and
  block offsets, patches block-target fixups, and turns symbol fixups into
  relocations using the target's `ObjectFormatInfo` (ELF machine, class,
  flags, op-name → relocation type mapping).
- **The ELF emitter/parser** (`write_elf`/`parse_elf`) serialize the
  format-neutral `ObjectFile` for both ELF32 and ELF64 (little-endian,
  `RELA` relocations). `tir readobj` dumps any relocatable ELF as
  FileCheck-friendly text; `--filetype=obj-ascii` renders instruction bytes
  as bracketed lists for lit tests.

Targets opt in through `TargetMachine`: `object_format()`,
`binary_writer()`, plus `pre_ra_lowerings()` (e.g. `vcond_br` →
`bne`+`vbr`, constant materialization — before register allocation because
the allocator must color the operands) and `finalize_lowerings()`
(`vret`/`vbr` → real instructions — after it because the allocator matches
`vret` by name).

## Emitting and linking an object

```sh
# C → object via fcc (riscv64 or arm64):
fcc compile --stage obj --march riscv64 -o add3.o add3.c

# TIR or assembly → object via tir mc:
tir mc --march=rv64i --filetype=obj -o caller.o caller.S

# Inspect:
tir readobj add3.o
llvm-objdump -dr add3.o
```

Linking with a standard linker, e.g. against a clang-built caller
(`int add3(int, int);` called from C):

```sh
# riscv64: match the soft-float ABI (fcc emits e_flags = 0 / lp64):
clang -target riscv64-linux-gnu -march=rv64i -mabi=lp64 -mno-relax -c caller.c
ld.lld caller.o add3.o -e caller -o linked.elf

# arm64:
clang -target aarch64-linux-gnu -c caller.c
ld.lld caller.o add3.o -e caller -o linked.elf
```

`llvm-objdump -d linked.elf` shows the `jal`/`bl` resolved into the
fcc-emitted function.

## Current limits

- `.text` only; no data sections or globals.
- Calls exist at the machine-IR level (`jal`/`bl` to a symbol); there is no
  `builtin.call` op and fcc cannot parse call expressions yet.
- The RISC-V `call` pseudo (`auipc`+`jalr`, `R_RISCV_CALL_PLT`) is not
  implemented; `jal` covers ±1 MiB.
- Block arguments on branch edges (phi values) are rejected by codegen, so
  fcc functions with control-flow merges do not lower yet.
- AArch64 scaled load/store offsets encode incorrectly for non-zero
  immediates (TMDL models byte offsets; the hardware field is scaled), so
  spilling is not safe on arm64 until that is fixed.
