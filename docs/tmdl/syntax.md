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
  - Range (inclusive, high bit first): `imm[4..0]` (selects bits 4–0)
- Register-operand projections: `rs1.value` is the bits the register holds,
  `rs1.index` the architectural register number the encoding spells. A bare
  operand means whichever the context is about — the value in a behavior, the
  index in an encoding — so both are only written when the two are mixed.
- Casts: `x as bits<N>` — the low `N` bits of `x`.
- Calls: `foo(a, b)` — a `fn` helper, inlined before anything else sees it.
- Grouping: `(expr)`
- Concatenation: `(a, b, c)` is `a` in the top bits, then `b`, then `c`; `()`
  is the empty bit vector, which contributes no bits. Its width is the sum of
  its elements'.
- Binary operators and precedence:
  - Highest: `*` `/`
  - Next: `+` `-` `|` `&` `^` `<<` `>>` (these share the same precedence tier)

### Typing

Widths are inferred; only the lossy conversions are written down.

- A literal has no width of its own: it takes one from where it is used, so
  `rs1 + 1` adds one to a register value and `self.XLEN - 1` is a spec-time
  `Integer`. A literal in an encoding is the exception — there the bits it
  spells *are* the field, so `0b000` is three bits and `0x8b` is eight.
- A narrower value reaches a wider one by zero-extension, with nothing written.
  Sign extension is `sext(x, w)` and narrowing is `x as bits<N>`; neither ever
  happens implicitly. A value whose width the spec never pins down — a register
  operand, whose class takes its `WIDTH` from an ISA parameter — cannot be used
  where a narrower width is expected without saying so.
- `bits<1>` is the condition type. There is no `bool`: an `if` takes a
  comparison, a single bit, or `&`/`|`/`!` over them.
- A `let` may name the width of its binding — `let low: bits<8> = ...` — and
  the bound value must fit in it.
- The `N` in `bits<N>`, whether in a cast or a `let`, is a literal: a cast keeps
  the low `N` bits, so a width the spec cannot pin down would not say which. A
  `fn` parameter works — `x as bits<n>` inside a helper — because calls are
  inlined before type checking.
- `width(x)` is the width `x`'s type declares, as a spec-time `Integer`. It is
  substituted before lowering, so it needs a declared type to read: an operand,
  a parameter, a slice, a cast or an annotated `let`, not a computed value.

Blocks and if‑expressions are supported for richer constructs:

```
{
  a = b + c;
  a // last expression returns value if no trailing semicolon
}

if cond { { ... } } else { { ... } }
```

## Top‑Level Items

TMDL files contain a sequence of items in any order:

- `isa` — defines an ISA/feature and its parameters.
- `register_class` — defines physical registers for one or more ISAs.
- `template` — reusable instruction template with parameters/operands/encoding/asm.
- `instruction` — concrete instruction (may inherit from a template) with behavior.
- `fn` — a pure expression-level helper, inlined at call sites (see below).

### Function Helpers

```
fn saturating_add(a, b) {
    sum = zext(a, self.XLEN + 1) + zext(b, self.XLEN + 1);
    if extract(sum, self.XLEN, self.XLEN) == zext(0b1, 1) { sext(0b1, self.XLEN) } else { extract(sum, self.XLEN - 1, 0) }
}

instruction AddSat for [RV32I] : RType {
    ...
    behavior { rd = saturating_add(rs1, rs2); }
}
```

A `fn` item declares a helper usable in `behavior` bodies (including inside
`map`/`reduce` lambdas) and in `encoding` expressions. A parameter may carry a
type — `fn rex(w: bits<1>, reg: bits<4>, rm: bits<4>)` — so an encoding
helper spells its own width and no call site has to supply it. Calls are
**inlined on the AST** before semantic
analysis: the body is substituted with the argument expressions at each call
site, so there is no runtime call and nothing downstream (instruction
selection, verification, codegen) is aware of helpers. Bodies may use locals,
`if`/`else`, and everything else expressions allow; free names (operands,
register paths, `self` parameters) resolve in the *caller's* scope. Functions
may call functions, but recursion is rejected, as is an arity mismatch.
Assignment to a fresh name inside a behavior block introduces a local binding
visible to later statements of the same block.


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
    x1("ra") => {},
    x2..x31 => {},
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
- Known traits currently recognized by tools: `hardwired_zero`, `program_counter`,
  `status_flag`, `float`, and `polymorphic`. Calling-convention traits such as
  `argument`, `caller_saved`, and `stack_pointer` moved to top-level `abi` items.
- `status_flag` marks condition-code bits (x86 EFLAGS `zf`, AArch64 PSTATE `z`):
  1-bit registers written as side effects by compare-style instructions
  (`EFLAGS::zf = ...` in a behavior) and read by conditional-branch guards
  (`if EFLAGS::zf == zext(0b1, 1) { PC::pc = ... }`). Instruction selection
  derives compare+branch rules from these classes (see the instruction
  selection design doc); flag registers carry no encoding slots and are never
  allocated.

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
    x31("sp") => {},   // overrides GPR's xzr at slot 31
  }
}
```

Operands then bind to the precise class (`rn: GPRsp` vs `rn: GPR`), and assembly
printing resolves each operand's register name through its own class.

### ABI

An `abi` describes stack layout, architectural roles, value-passing sequences,
and saved/reserved registers independently of the register classes:

```
abi LP64("lp64") for [RV64I] {
  stack { align = 16; grows = down; red_zone = 0; slot_size = 8; }
  sp = GPR::x2;
  ra = GPR::x1;
  fp = GPR::x8;
  args int -> [GPR::x10..GPR::x17], then stack;
  rets int -> [GPR::x10..GPR::x11];
  callee_saved = [GPR::x2, GPR::x8..GPR::x9, GPR::x18..GPR::x27];
  reserved = [GPR::x0, GPR::x3, GPR::x4];
  classifier = riscv;
}
```

An ABI may inherit another with `: Base`; stack fields and omitted lists inherit,
while argument/return sequences and roles declared by the child replace matching
entries from the base.

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
    FUNCT7,
    rs2,
    rs1,
    FUNCT3,
    rd,
    OPCODE,
  }

  asm { "{self.MNEMONIC} {rd}, {rs1}, {rs2}" }
}
```

- `operands` — operand name to type mapping (typically to a register class or a bitvector type).
- `encoding` — the instruction word as a list of fields, high bit first (see
  [Encoding Section Details](#encoding-section-details)).
  - A field can be an operand, a parameter, a bit slice (e.g. `imm[4..0]`) or a
    literal; each carries its own width.
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
- `behavior` — required; describes semantics using the expression language. Statements execute in order. Operand and fixed-register reads start as instruction-entry snapshots; assigning a name updates that name for later statements without changing a different operand that aliases the same physical register. Basic assignments and arithmetic/bitwise ops are supported.
- Builtin functions usable in behaviors: `sext`/`zext` (width extension),
  `extract`, `width`, `clamp`, `log2Ceil`, `load`/`store` (memory), and `trap(cause)` —
  raise a synchronous exception with a constant cause code (e.g. RISC-V
  `ecall`/`ebreak`); the simulator routes it to its exception callback.
- Atomic memory and fence builtins (an optional trailing `Ordering::*` argument
  selects acquire/release semantics; see
  [Memory Model](memory_model.md) for the full reference):
  - `load_reserved(addr, bytes, ordering)` — read memory and register a
    reservation over the accessed range.
  - `store_conditional(addr, bytes, value, ordering)` — write iff the
    reservation still covers the exact range; evaluates to `bits<1>` (1 success,
    0 failure) and consumes the reservation.
  - `atomic_rmw(op, addr, bytes, value, ordering)` — one atomic
    read-modify-write, evaluating to the old memory value; `op` is one of
    `add, swap, xor, and, or, min, max, minu, maxu`.
  - `fence(pred, succ)` — data-memory ordering fence; `fence_i()` —
    instruction-stream fence. Both are statements, like `trap`.
- Functional vector builtins operate on iterators (a value split into lanes):
  - `split(bits, n)` — cut a bit value into `n` equal-width lanes, lane 0 from
    the low bits.
  - `concat(iter)` — the inverse: join an iterator's lanes into one bit value.
  - `map(iter, |x| ...)` — apply a lambda to each lane.
  - `zip(a, b)` — pair two iterators lane-wise, so a binary `map` lambda
    (`map(zip(a, b), |x, y| ...)`) reads both sides as separate parameters.
    Accepts more than two iterators: a `map` over `zip(a, b, c)` takes a
    three-parameter lambda (`|x, y, z| ...`), one parameter per zipped
    iterator.
  - `reduce(iter, |acc, x| ...)` — left-fold a binary lambda over the lanes
    (e.g. a horizontal add).
  - `iota(n, w)` — an iterator of `n` lanes of `w` bits holding the lane
    indices 0..n-1. Zip it with another iterator to give a `map` lambda
    positional awareness (lane index alongside lane value).
  - Lambdas use Rust syntax — `|x| body` or `|a, b| body` — and are valid only
    as the function argument of `map`/`reduce`. A lane-wise vector add is
    `concat(map(zip(split(vs2, n), split(vs1, n)), |a, b| a + b))`.
  - Lane values may be sub-byte wide (e.g. a mask register split into 1-bit
    lanes); the simulator packs and unpacks them bit by bit.
  - An inline conditional `if cond { a } else { b }` (single-expression arms,
    mandatory `else`) is available in value positions such as lambda bodies —
    e.g. a masked lane update is
    `map(zip(mask, new, old), |m, n, o| if m { n } else { o })`.
- Optional `asm`/`encoding` sections can be provided or inherited.

## Encoding Section Details

An `encoding` lists the fields of the instruction word the way the ISA's manual
draws them: high bit first within an **encoding unit**, units in emission order.
The unit is the ISA's `ENCODING_UNIT` parameter — 32 for a fixed-width word ISA
like RISC-V or AArch64, 8 for a byte-stream ISA like x86-64, whose manual draws
one byte at a time. With no `ENCODING_UNIT` declared the whole encoding is a
single unit. A field wider than one unit spans whole units and is little-endian
across them, which is how a multi-byte displacement or immediate reaches memory.

Every field carries its own width, so the fields together determine the
instruction's size and no bit position is ever written down:

- An operand or parameter contributes its declared width — `bits<N>`, or the
  `ENCODING_LEN` of a register operand's class.
- A bit slice contributes its span: `imm[4..0]` is five bits, `imm[11]` is one.
  An operand whose width is an ISA-parameter expression (the RV32/RV64
  `bits<log2Ceil(self.XLEN)>` shift amount) has no single width, so it must be
  sliced explicitly.
- A literal contributes the bits it spells: one per binary digit and four per
  hex digit, so `0b000` is three bits and `0x8b` is eight. A decimal literal
  spells no width and is rejected. Underscores group digits: `0b0000_1111`.

Register operands can be sliced, splitting one register across several fields.
x86-64 r8..r15 put their 4th number bit in the REX prefix and the low three in
ModR/M, so with `ENCODING_UNIT = 8` a register-to-register instruction reads as
the manual's `0100WRXB`, opcode, `mod reg r/m`:

```
encoding {
  0b0100, REXW, src[3], 0b0, dst[3],
  OPCODE,
  0b11, src[2..0], dst[2..0],
}
```

Example from `tmdl/checks/Inputs/simple.tmdl`:

```
encoding {
  0x0000,
  rd,
  rs2,
  rs1,
  0b1,
}
```

### The Encoding Is an Expression

`encoding { a, b, c }` is sugar for the concatenation `(a, b, c)`, so anything
that produces bits can stand where a field does: a nested concatenation, a call
to a `fn` helper, or an `if` whose arms differ in width. A nested concatenation
is a group the manual draws as whole encoding units, so it must fill them.

```
fn rex(w: bits<1>, reg: bits<4>, rm: bits<4>) {
  if w | reg[3] | rm[3] { (0b0100, w, reg[3], 0b0, rm[3]) } else { () }
}

fn modrm_reg(reg: bits<4>, rm: bits<4>) { (0b11, reg[2..0], rm[2..0]) }

encoding { rex(0b1, src, dst), OPCODE, modrm_reg(src, dst) }
```

### Shapes

An encoding with conditions is not one bit map but several. The compiler
inlines the helpers, substitutes the `let` bindings and expands the conditions
into **shapes**: each truth assignment of the condition set that some operand
value produces yields one fixed bit map with the guard that selects it. The
example above has two — one 3 bytes with the REX prefix, one 2 bytes without —
and `w = 0b1` makes the second unreachable. An encoding with no condition has
exactly one shape, always taken, which is what every fixed-width ISA has.

The rules a set of shapes must satisfy:

- A condition is `bits<1>` and is decided from the instruction's own operands
  and parameters; nothing else is in hand when the encoder runs.
- At least one shape is reachable, and every operand value the expansion tries
  selects exactly one. An operand its `#[align]`/`#[nonzero]` constraints leave
  no value for makes them all unreachable.
- Shapes are decode-distinguishable: any two differ in width or in some bit
  both of them fix, so decoding stays a function of the instruction word.
- An instruction whose behavior reads the program counter has one shape; its
  behavior cannot see which one the encoder picked.
- At most eight conditions per encoding. Beyond that the encoding is a design
  smell, not a limit to raise.

A condition its parameters already answer is not a shape. One template writes
the encoding of every width it serves, and the instruction's parameters decide
which branches it takes, so `if REXW | reg[3] | rm[3] { … }` with `REXW = 0b1`
is the prefix unconditionally and one shape, while `REXW = 0b0` leaves the
register test and two shapes. The dead branch is gone before the expansion
runs, and the guard the encoder carries reads operands only.

Reachability is decided by evaluating the conditions over the operand domains.
An operand up to 8 bits wide is enumerated, so the answer is exact. A wider one
is sampled: the boundaries, every single-bit value, every constant the
conditions name, and for each `x[hi..lo] == k` they spell, values that satisfy
it with the rest of the operand zero and all-ones. Two conditions over disjoint
slices of one wide operand can still be missed together, which drops a shape, so
the encoder-decoder agreement per shape is also an SMT obligation.

`emit-rust` lowers a guarded encoding to one `EncodeShape` per bit map, and the
AST and JSON actions export the shapes. The symbolic emitters still describe one
bit map per instruction: an instruction with more than one shape is a `tmdlc`
error for `emit-smtlib`, `emit-btor2` and `emit-markdown`.

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
    x10..x17("a{}") => {},
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
