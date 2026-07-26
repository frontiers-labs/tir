# Data Layout and Target Environment

A module needs two kinds of target facts to be lowered, and they belong to
different owners:

- **`data_layout`** — the software half of the ABI: byte order, the size and
  alignment of each data type, the stack alignment. Two toolchains for the same
  chip can disagree about it (ILP32 versus LP64 pointers on the same core).
- **`target_env`** — the hardware half: which architecture the code runs on, its
  enabled ISA extensions, optionally the CPU. Facts fixed by the chip.

Both are plain dictionary attributes, so they are carried in the IR itself
rather than only in the driver's flags:

```tir
#data_layout = {endianness = "little", stack_alignment = 128, types = {i32 = {abi = 32}, p = {abi = 64, size = 64}}}
#target_env = {arch = "riscv64", features = ["rv64i", "rvm", "c"]}

module {data_layout = #data_layout, target_env = #target_env} {
  func @f() -> !i32 {
    %0 = constant {value = 1} : !i32
    return %0
  }
  module_end
}
```

Every size and alignment is in **bits**, matching the width of an `!i32`.

## Scopes

A spec may sit on any operation and applies to everything nested inside it. The
value in force at an operation is the merge of its own spec with those of its
enclosing operations, **innermost winning**:

- nested dictionaries merge key by key, so a function can override the alignment
  of one type and inherit the rest;
- every other value — a scalar, an array — is replaced whole, so an inner
  `features` list replaces the outer one instead of adding to it.

The target selected by `--march` is the outermost scope of all: its own
description (`TargetMachine::data_layout` / `target_env`) applies wherever the IR
declares nothing, and any entry the IR does declare overrides it. That is why
existing IR without a spec still lowers, and why a module can opt into 32-bit
pointers on a 64-bit target by declaring `p` alone.

Producers currently attach specs to the module only: `fcc` records the layout and
environment of the target it compiled for.

## `data_layout` entries

| Entry | Value |
| --- | --- |
| `endianness` | `"little"` or `"big"` |
| `stack_alignment` | alignment the stack pointer is kept at, in bits |
| `types` | one entry per layout class, keyed as below |

A type entry is keyed by the type's layout *class*, not its full spelling:
`i{width}` for integers, the mnemonic (`f32`, `bf16`) for floats, `p` for
pointers. Each holds `size`, `abi` and optionally `preferred` (defaulting to
`abi`); `size` defaults to the type's own width, so only pointers must state it.
Lookup is by exact key: a class the spec does not declare has no layout, rather
than borrowing a wider entry's.

## `target_env` entries

| Entry | Value |
| --- | --- |
| `arch` | target architecture, spelled as `--march` accepts it |
| `cpu` | CPU name, when one is selected |
| `features` | enabled ISA extensions, as `--mattr` names them |

`tir mc` reads `arch` when no `--march` is given, so a module that describes
itself needs no flags:

```sh
tir mc --stage=isel self-describing.tir
```

## Extension keys

Entries outside the tables above are carried through untouched and readable with
`DataLayout::get` / `TargetEnv::get`. That is where a dialect puts metadata only
it understands — a GPU's address-space mapping or shared memory size, a cache
hierarchy — without changing the core:

```tir
#target_env = {arch = "ptx", shared_memory = 65536}
```

Verification checks the shape of the predefined entries (a `data_layout` must be
a dictionary, `endianness` must name a byte order, a type entry must hold bit
counts under known field names) and leaves extension keys alone.

## Reading a spec

```rust
let layout = DataLayout::for_op(context, op)?;
let bits = layout.size_in_bits(context, ty)?;
let align = layout.abi_alignment(context, ty)?;

// Passes that also have a target machine resolve against its description, so
// the target supplies whatever the IR leaves out:
let layout = DataLayout::for_op_with_default(context, op, target.data_layout().as_ref())?;
```

Instruction selection uses exactly that call: the width of a pointer is a layout
fact, so `!ptr.p` values are loaded and stored at the width the layout in scope
declares, falling back to the target's own.

Any dialect can define its own scoped metadata key with `tir::scoped_dict`,
which is the primitive both queries are built on.

## Aliases

A spec is too long to read inline, so the parser accepts file-scoped aliases —
`#name = <value>` ahead of the top-level operation — usable in any attribute
position. Printing reverses the transformation: `tir::print_ir` hoists every
`data_layout` and `target_env` dictionary into an alias definition named after
the attribute, so a printed module stays as readable as a hand-written one and
re-parses unchanged.
