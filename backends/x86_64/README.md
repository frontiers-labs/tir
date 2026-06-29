# tir-x86_64 (prototype)

A prototype x86-64 backend built, like the other backends, from TMDL
descriptions in [`defs/`](./defs). It explores how far TMDL reaches for a
variable-length CISC ISA.

## Scope

- **Registers:** the eight base 64-bit GPRs (`rax`..`rdi`), 64-bit width.
- **Instructions** (behavior-defined, roughly the RISC-V "I" integer subset):
  - reg/reg ALU: `add` `sub` `and` `or` `xor` `mov`
  - reg/imm32 ALU: `add` `and` `or` `xor` `mov`
  - shift by imm8: `shl` `shr` `sar`
  - control flow: `jmp` `call` `ret`
- **Syntax:** Intel (`op dst, src`). The shared assembler lexer has no `%`/`$`
  sigils, so AT&T operands do not lex; Intel's bare registers/immediates do.

## Not expressible

The blocked x86 features and the exact TMDL limitation for each are in
[`BLOCKERS.md`](./BLOCKERS.md). In short: extended registers `r8`..`r15` (REX
register-field split), all memory operands (variable-length ModR/M+SIB and the
missing memory syntax), and the `imm64`/segment/absolute MOV forms.

## Tests

```
cargo test -p tir-x86_64
```

- `tests/roundtrip.rs` — assembly parse → print roundtrip.
- `tests/encoding.rs` — assemble → machine-code bytes, checked against the real
  x86-64 encodings.
