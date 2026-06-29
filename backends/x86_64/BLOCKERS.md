# x86-64 in TMDL: what blocks integration

This prototype models the eight base 64-bit GPRs and a RISC-V-"I"-flavored
subset of behaviors (reg/reg ALU, reg/imm ALU, immediate shifts, `jmp`/`call`/
`ret`) in Intel syntax. Everything below is x86 that the **current** TMDL cannot
express, with the concrete reason. Line references are to the TMDL compiler.

## 1. Extended registers r8–r15 (REX.R / REX.B register-field split)

x86-64 register numbers are 4 bits: the low 3 go in the ModR/M `reg`/`rm` field,
the 4th goes in the REX prefix (REX.R for `reg`, REX.B for `rm`). Encoding one
register operand therefore requires **splitting it across two non-adjacent byte
fields**.

The TMDL `encoding` section can place an operand whole, or slice/index an
`Integer`/`bits<N>` operand — but it rejects slicing/indexing a **register**
operand (`rustgen.rs` ~3051–3084: `Type::Struct` is allowed only as a whole
field). There is no way to write `rm[0..2]` into ModR/M and `rm[3]` into REX.

Consequence: the GPR class is limited to indices 0–7 (`rax`..`rdi`), addressable
by the 3-bit field with a fixed `REX.W = 0x48`. Adding `r8`..`r15` would silently
truncate the 4-bit index to 3 bits and emit the wrong register, so they are left
out rather than shipped wrong.

*Fix shape:* let `encoding` slice register operands (treat a register like a
`bits<ENCODING_LEN>` value).

## 2. Memory operands: ModR/M + SIB + variable-length displacement

An instruction's machine-code length in TMDL is fixed: the encoder width is
`ceil(highest_bit / 8)` bytes (`rustgen.rs` ~735, ~2983). One instruction = one
fixed-width bit layout.

x86 memory operands are variable-length and operand-value-dependent:
- ModR/M `mod` selects no displacement / `disp8` / `disp32`;
- `rm = 100` adds a SIB byte;
- `mod = 00, rm = 101` is RIP-relative (a `disp32`).

The length depends on the chosen addressing mode, which TMDL cannot vary per
operand value. So no load/store and no memory operand is expressible — this
removes the `r/m`-memory half of MOV and of every ALU op.

*Fix shape:* a variable-length / optional-field encoding model (alternatives
selected by operand shape).

## 3. Memory-operand assembly syntax

The shared assembler lexer (`backends/common/src/lexer.rs`) only tokenizes
`,` `(` `)`, identifiers, numbers, strings and directives. It has **no** `%`,
`$`, `[`, `]`, `*`.

- Intel memory `[base + index*scale + disp]` needs `[` `]` `*` `+` — none lex.
- AT&T memory `disp(base,index,scale)` parenthesizes fine but needs `%`/`$` for
  its registers/immediates — none lex.

So memory operands cannot even be parsed, independent of #2. (This same gap is
why **Intel** syntax was chosen for the register/immediate forms: AT&T's `%rax`
/`$1` sigils do not lex, while Intel's bare `rax`/`1` are exactly what the lexer
already produces.)

*Fix shape:* extend the lexer and the asm-template grammar with a memory-operand
form.

## 4. One operation per mnemonic

TMDL dedups instructions by resolved operation name and errors on a collision
(`sema.rs` ~699–710). x86 deliberately overloads one mnemonic across many
distinct behaviors. The prototype works around it by giving each form a distinct
`OPNAME` while keeping `MNEMONIC` shared for assembly — so the assembler still
dispatches `add` to both the reg and imm forms. This is friction rather than a
hard wall, but it means the "MOV is 7 instructions" reality must be spelled as 7
`OPNAME`s.

## 5. Encoding is mandatory

Every instruction must define an encoding (`sema.rs` ~712–720); there is no
behavior-only/pseudo instruction. Combined with #1–#2, an instruction whose
encoding is not expressible cannot be added even as behavior + asm only — it must
be omitted entirely. That is why this prototype excludes the blocked forms rather
than shipping them without encodings.

## The seven MOVs (the example from the task)

MOV's behaviors, and where each lands:

| Form | Opcode | Status |
|------|--------|--------|
| `MOV r/m64, r64` | `89 /r` | prototyped (`Mov`) |
| `MOV r64, r/m64` (load) | `8B /r` | blocked: memory operand (#2, #3) |
| `MOV r/m64, imm32` | `C7 /0` | reg form prototyped (`MovImm`); mem form blocked (#2, #3) |
| `MOV r64, imm64` | `REX.W B8+rd` | blocked: register number lives in the opcode byte + REX.B (#1) |
| `MOV Sreg, r/m` / `MOV r/m, Sreg` | `8E` / `8C` | out of scope: no segment register class |
| `MOV rax, moffs64` / `MOV moffs64, rax` | `A1` / `A3` | blocked: 8-byte absolute address operand + no syntax (#2, #3) |
| `MOV r/m, CR/DR` | `0F 20`.. | out of scope: control/debug registers |

## Expressible, just out of scope (not blockers)

- **Flags (EFLAGS) for `cmp`/`test`/`jcc`/`setcc`/`adc`/`sbb`.** Expressible:
  AArch64 already models `PSTATE { n, z, c, v }` as a `register_class` and writes
  it from behaviors; x86 condition codes would follow the same pattern. Omitted
  only to keep the prototype to the flag-free RISC-V-I shape.
- **32/16/8-bit operand sizes** (no REX / `66` prefix / distinct opcodes).
  Expressible as separate instructions with their own fixed encodings; it just
  multiplies the instruction count.
