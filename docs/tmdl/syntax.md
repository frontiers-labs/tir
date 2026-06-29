# TMDL Syntax Guide

This document describes the syntax of the TIR Machine Description Language (TMDL).
It is intended as a concise, example‑driven reference. For background and goals,
see the Motivation section of the [docs](index.md).

## Lexical Elements

- Whitespace and newlines are insignificant except inside string literals.
- Line comments start with `//` and run to end of line.
- Identifiers: ASCII identifiers (`[A-Za-z_][A-Za-z0-9_]*`).
- String literals: double‑quoted, no escape sequences yet: `"text"`.
- Numbers:
  - Decimal: `0`, `42`, `1234`
  - Hex: `0x1f`, `0XDEAD` (also supports a leading `-`)
  - Binary: `0b1010` (decimal/hex/binary are available in expressions)
- Punctuation and operators used across the grammar: `{ } [ ] ( ) , : ; => .. = + - * / & ^ | . < >`

## Types

- `Integer` — unbounded signed integer for spec‑time calculations.
- `String` — string literal values.
- `bits<N>` — fixed width unsigned bitvector (e.g., `bits<7>`).
- Struct type — names a user‑defined type such as a register class: `GPR`.

## Expressions

Expressions are used in parameters, encodings, asm templates, and behavior.

- Literals: numbers and strings as above.
- Identifiers and field access: `self.MNEMONIC`, `imm`.
- Indexing and slicing on bitvectors:
  - Single bit/index: `imm[11]`
  - Range (inclusive): `imm[0..4]` (selects bits 0–4)
- Calls: `foo(a, b)` (reserved for future extensions).
- Grouping: `(expr)`
- Binary operators and precedence:
  - Highest: `*` `/`
  - Next: `+` `-` `|` `&` `^` `<<` `>>` (these share the same precedence tier)

Blocks and if‑expressions are supported for richer constructs:

```
{
  a = b + c;
  a // last expression returns value if no trailing semicolon
}

if cond { { ... } } else { { ... } }
```

`for` loops repeat a block over a half‑open integer range, binding the loop
variable to each value:

```
for i in 0..n {
  rd = rd + i;
}
```

A loop whose body is a single accumulating assignment (`dest = step`, where
`step` may read `dest` and the loop variable) lowers to a first‑class `Loop`
node in the semantic‑expression graph: a fold of `step` over the range, with
`dest` as the accumulator. The interpreter evaluates it natively, so the bounds
may be symbolic (depend on operands). Backends without native iteration — the
SMT backend — unroll it, which requires the bounds to be compile‑time constants
(literals, ISA/instruction parameters such as `self.XLEN`, or arithmetic over
them); a symbolic‑bound loop is reported as unsupported there.

Other loop shapes (multiple statements, memory effects) are not folds; they are
unrolled at compile time and therefore require constant bounds.

## Top‑Level Items

TMDL files contain a sequence of items in any order:

- `isa` — defines an ISA/feature and its parameters.
- `register_class` — defines physical registers for one or more ISAs.
- `template` — reusable instruction template with parameters/operands/encoding/asm.
- `instruction` — concrete instruction (may inherit from a template) with behavior.

### ISA Definition

```
isa RV32I {
  param XLEN: Integer = 32;
}
```

- Optional requirements declare dependencies on other ISAs/features:
  - Single: `requires RV64I`
  - Any of: `requires [RV32I | RV64I]` (pipe‑separated inside brackets)
  - All of: `requires [Foo, Bar]` (comma‑separated inside brackets)

### Register Class

```
register_class GPR for [RV32I, RV64I] {
  param ENCODING_LEN: Integer = 5;
  param WIDTH: Integer = self.XLEN;

  registers {
    x0("zero") => { traits = [hardwired_zero] },
    x1("ra")   => { traits = [return_address, caller_saved] },
    x2..x7("t{}") => { traits = [caller_saved] },
    x8..x9("s{}")  => { traits = [callee_saved] },
    x10..x17("a{}") => { traits = [caller_saved] },
    x18..x27("s{}") => { traits = [callee_saved] },
    x28..x31("t{}") => { traits = [caller_saved] },
  }
}
```

- `for [...]` — attach the class to multiple ISAs.
- Single register: `name("alias") => { traits = [..] }`
- Range: `start..end("alias{}") => { traits = [..] }` uses `{}` placeholder to number aliases sequentially.
- Explicit encoding index: `name => { index = 0xC00 }` for registers whose
  architectural number is not derivable from the name (e.g. RISC-V CSRs).
  Without it, the index is the trailing number in the name (`x5` -> 5), or the
  declaration position for index-less registers. Both `index` and `traits` are
  optional inside the braces.
- Known traits currently recognized by tools: `hardwired_zero`, `return_address`, `caller_saved`, `callee_saved`, `stack_pointer`. Other identifiers parse but may be ignored by current tooling.

#### Inheritance

A class may inherit another with `: Base`. It absorbs the base's parameters and
registers, then applies its own declarations as overrides — parameters by name,
registers by encoding index (or by name for index-less registers). The two classes
name the **same physical register file**: a given encoding index is the same
register in both, so the register allocator treats their indices as aliases. This
expresses architectures where one encoding slot denotes different registers in
different operand positions — e.g. AArch64 encoding `31` is the zero register in
most operands but the stack pointer in addressing bases and add/sub-immediate:

```
register_class GPRsp for [ARMv8A64] : GPR {
  registers {
    x31("sp") => { traits = [stack_pointer] },   // overrides GPR's xzr at slot 31
  }
}
```

Operands then bind to the precise class (`rn: GPRsp` vs `rn: GPR`), and assembly
printing resolves each operand's register name through its own class.

### Instruction Template

```
template RType for [RV32I, RV64I] {
  param MNEMONIC: String;
  param FUNCT7: bits<7>;
  param FUNCT3: bits<3>;
  param OPCODE: bits<7>;

  operands {
    rd: GPR,
    rs1: GPR,
    rs2: GPR,
  }

  encoding {
    0..6  => OPCODE,
    7..11 => rd,
    12..14 => FUNCT3,
    15..19 => rs1,
    20..24 => rs2,
    25..31 => FUNCT7,
  }

  asm { "{self.MNEMONIC} {rd}, {rs1}, {rs2}" }
}
```

- `operands` — operand name to type mapping (typically to a register class or a bitvector type).
- `encoding` — bitfield layout using single bits `i => expr` and ranges `i..j => expr`.
  - Right‑hand side can reference operands, parameters, slices (e.g., `imm[0..4]`).
- `asm` — expression producing assembly syntax. Today commonly a string template with placeholders:
  - `{self.MNEMONIC}` resolves to the instruction mnemonic (from parameters).
  - `{name}` inserts the textual form of operand `name` (registers and immediates).

Inheritance:

```
template LoadInst for [RV32I, RV64I] : IType {
  param OPCODE: bits<7> = 0b0000011;
  asm { "{self.MNEMONIC} {rd}, {imm}({rs1})" }
}
```

Use `: ParentTemplate` to inherit parameters/operands/encoding; you can override/add members.

### Instruction Definition

```
instruction Add for [RV32I, RV64I] : RType {
  param MNEMONIC: String = "add";
  param FUNCT3: bits<3> = 0b000;

  behavior { rd = rs1 + rs2; }
}
```

- Same structure as `template` with optional inheritance and `for [...]`.
- `behavior` — required; describes semantics using the expression language. Basic assignments and arithmetic/bitwise ops are supported.
- Builtin functions usable in behaviors: `sext`/`zext` (width extension),
  `extract`, `clamp`, `log2Ceil`, `load`/`store` (memory), and `trap(cause)` —
  raise a synchronous exception with a constant cause code (e.g. RISC-V
  `ecall`/`ebreak`); the simulator routes it to its exception callback.
- Functional vector builtins operate on iterators (a value split into lanes):
  - `split(bits, n)` — cut a bit value into `n` equal-width lanes, lane 0 from
    the low bits.
  - `concat(iter)` — the inverse: join an iterator's lanes into one bit value.
  - `map(iter, |x| ...)` — apply a lambda to each lane.
  - `zip(a, b)` — pair two iterators lane-wise, so a binary `map` lambda
    (`map(zip(a, b), |x, y| ...)`) reads both sides as separate parameters.
  - `reduce(iter, |acc, x| ...)` — left-fold a binary lambda over the lanes
    (e.g. a horizontal add).
  - Lambdas use Rust syntax — `|x| body` or `|a, b| body` — and are valid only
    as the function argument of `map`/`reduce`. A lane-wise vector add is
    `concat(map(zip(split(vs2, n), split(vs1, n)), |a, b| a + b))`.
- Optional `asm`/`encoding` sections can be provided or inherited.

## Encoding Section Details

- Locations use bit indices where `0` is least significant bit.
- Range `a..b` covers bits `[a, b]` inclusive; e.g., `7..11` covers bits 7,8,9,10,11.
- RHS expressions can be immediates, parameters, operand values, slices/indexes of bitvectors.
- Register operands can be sliced too, splitting one register across several
  fields. x86-64 r8..r15 put their 4th number bit in the REX prefix and the low
  three in ModR/M: `0 => dst[3]` and `16..18 => dst[0..2]`.

Examples from `tmdl/checks/Inputs/simple.tmdl`:

```
encoding {
  0 => 1,
  1..5 => rs1,
  6..10 => rs2,
  11..15 => rd,
  16..31 => 0,
}
```

## ASM Templates

Most current templates and instructions provide a single string literal:

```
asm { "{self.MNEMONIC} {rd}, {rs1}, {rs2}" }
asm { "{self.MNEMONIC} {rd}, {imm}({rs1})" }
```

- Placeholders:
  - `{self.MNEMONIC}` — mnemonic from template/instruction parameters.
  - `{op}` — an operand placeholder by name (e.g., `rd`, `rs1`, `imm`).
- Additional logic (e.g., Intel vs AT&T syntax selection) can be expressed with full expressions/blocks; today, simple literal templates are the norm.

## Feature Scoping and Requirements

- `for [A, B]` after `register_class`, `template`, or `instruction` limits applicability to those ISAs/features.
- `requires ...` inside `isa` defines dependencies:
  - `requires Foo` — single requirement.
  - `requires [Foo | Bar]` — any of the listed features.
  - `requires [Foo, Bar]` — all listed features.

## Putting It Together (RISC‑V Excerpts)

From `backends/riscv/defs/main.tmdl`:

```
isa RV64I { param XLEN: Integer = 64; }

register_class GPR for [RV32I, RV64I] {
  param ENCODING_LEN: Integer = 5;
  param WIDTH: Integer = self.XLEN;
  registers {
    x0("zero") => { traits = [hardwired_zero] },
    x10..x17("a{}") => { traits = [caller_saved] },
  }
}

template RType for [RV32I, RV64I] {
  param MNEMONIC: String;
  ...
  asm { "{self.MNEMONIC} {rd}, {rs1}, {rs2}" }
}

instruction And for [RV32I, RV64I] : RType {
  param MNEMONIC: String = "and";
  param FUNCT3: bits<3> = 0b111;
  behavior { rd = rs1 & rs2; }
}
```
