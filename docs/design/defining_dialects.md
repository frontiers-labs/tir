# Defining Dialects

A dialect is a named IR vocabulary: a set of operations, types, interfaces, and
lowering or analysis hooks that describe one domain at a useful level of
abstraction. TIR keeps dialects small and composable. A frontend can start in
`builtin` plus `ptr`, an instruction selector can progressively replace those
operations with a target dialect such as `riscv`, and later passes can still
walk all of them through the same `Context`, `Operation`, `Type`, and `Pass`
interfaces.

There are two ways to get a dialect:

- Write it directly in Rust with `dialect!` and `operation!`.
- Generate a target-machine dialect from TMDL.

Use Rust for IR infrastructure and hand-written compiler dialects. Use TMDL for
large ISA descriptions where the operation set, asm parser, semantic expression,
register tables, and machine model should come from one source of truth.

## Dialect Registration

A Rust dialect is declared with `dialect!`:

```rust
use crate::{dialect, operation};

dialect! {
    PtrDialect {
        name: "ptr",
        operations: [
            AllocaOp,
            LoadOp,
            StoreOp,
        ],
        types: [PtrType],
    }
}
```

The macro creates the dialect struct and implements `tir::Dialect`. During
registration it installs:

- dynamic operation converters,
- text parsers for each operation,
- type parsers for each type,
- operation interface registrations.

Dialects are made available through a `Context`:

```rust
let context = tir::Context::with_default_dialects();
// or, for an empty context:
let context = tir::Context::new();
context.register_dialect::<PtrDialect>();
```

`Context::with_default_dialects()` registers the core dialects used by ordinary
IR: `builtin`, `ptr`, and `scf`.

## Defining Operations

Operations are declared with `operation!`. The macro emits the operation wrapper,
builder, parser, printer, verifier plumbing, and the convenience constructor
function used by most builders.

```rust
use crate as tir;
use crate::{MemoryRead, operation};

operation! {
    LoadOp {
        name: "load",
        dialect: "ptr",
        operands: O {
            ptr: "crate::ptr::PtrType",
        },
        results: R {
            result: "crate::Any",
        },
        interfaces: [MemoryRead],
    }
}

impl MemoryRead for LoadOp {
    fn read_location(&self) -> tir::ValueId {
        self.operands()[0]
    }

    fn read_value(&self) -> tir::ValueId {
        self.result()
    }
}
```

The generated builder is used directly:

```rust
let loaded = builder.insert(
    LoadOpBuilder::new(&context)
        .ptr(pointer_value)
        .result_type(i32_ty)
        .build(),
);
```

The macro also creates a lowercase helper named after the operation. For
operation names that are Rust keywords, use a raw identifier at the call site:

```rust
builder.insert(tir::builtin::ops::r#return(&context, value).build());
```

### Operation Sections

`operation!` accepts these sections:

```rust
operation! {
    AddIOp {
        name: "addi",
        dialect: "builtin",
        attributes: A {
            predicate: "Str",
        },
        operands: O {
            lhs: "crate::builtin::IntegerType",
            rhs: "crate::builtin::IntegerType",
        },
        results: R {
            result: "crate::builtin::IntegerType",
        },
        regions: R {
            body: Region {
                single_block: true,
            }
        },
        interfaces: [Commutative, SameOperandType],
        sem: "(set result (add lhs rhs))",
        format: "custom",
    }
}
```

Only `name` and `dialect` are required. Most operations use a subset of the
sections above.

- `attributes`: declares required attribute names and their expected attribute
  kinds. The generated verifier checks the shape.
- `operands`: declares ordered operands and their type constraints.
- `results`: declares result slots and their type constraints. Builders use one
  `result_type` field, or `result_types`/`result_values` for a variadic result
  group — which is what a machine instruction declares, one result per register
  slot it writes.
- `regions`: declares nested regions. A `single_block` region is auto-created by
  the builder when omitted.
- `interfaces`: registers dynamic operation interfaces with the `Context`.
- `sem`: attaches a semantic-expression lowering for instruction selection,
  rewriting, and simulation.
- `format: "custom"`: opts out of the default text parser/printer and expects
  `custom_print` and `custom_parse` methods on the operation type.

## Types

A dialect type is ordinary Rust implementing `tir::Type`; if it is usable as an
operation constraint, it also implements `tir::TypeConstraint`.

```rust
use std::any::Any;
use std::sync::Arc;

use crate as tir;
use crate::{
    Context, Error, IRFormatter, Type, TypeId, TypeConstraint, parse::Span,
};

pub struct PtrType {
    pointee: Option<Arc<dyn Type>>,
}

impl PtrType {
    pub fn opaque(context: &Context) -> TypeId {
        context.get_type_id(Arc::new(Self { pointee: None }))
    }
}

impl TypeConstraint for PtrType {}

impl Type for PtrType {
    fn dialect(&self) -> &'static str {
        "ptr"
    }

    fn parse_key() -> &'static str {
        "p"
    }

    fn parse<'src>(
        _mnemonic: &str,
        _parser: &mut tir::parse::text::Parser<'src>,
        context: &Context,
    ) -> Result<TypeId, (Span, Error)> {
        Ok(Self::opaque(context))
    }

    fn print(&self, fmt: &mut IRFormatter<'_>) -> Result<(), std::fmt::Error> {
        fmt.write("p")
    }

    fn eq(&self, other: &dyn Type) -> bool {
        (other as &dyn Any).downcast_ref::<PtrType>().is_some()
    }
}
```

Types are interned in the context with `Context::get_type_id`. Two type values
are considered the same when their `Type::eq` implementation says so.

Textual type syntax is dialect-qualified unless the type belongs to `builtin`.
For example, `!i32` is the builtin integer type and `!ptr.p` is the pointer type
from the `ptr` dialect.

## Interfaces

Interfaces let passes ask for behavior without depending on concrete operation
types. For example, `verify-deps` uses `PromotableAllocation`, `MemoryRead`, and
`MemoryWrite`; backend passes use `MachineInstruction`.

To expose an interface:

1. List it in the operation's `interfaces` section.
2. Implement the trait for the operation type.

The `operation!` macro emits the `ImplementsOpInterface` plumbing, and the
`dialect!` macro registers it when the dialect is installed in a context.

## Parsing and Printing

The default operation format is intentionally regular:

- optional result list,
- dialect-qualified operation name,
- optional attribute dictionary,
- optional region bodies.

Use the default format for compiler IR. Choose `format: "custom"` only when the
operation needs target assembly syntax or another non-generic shape. Custom
format operations must provide:

```rust
impl MyOp {
    fn custom_print<'a, 'b: 'a>(
        &'a self,
        fmt: &'a mut tir::IRFormatter<'b>,
    ) -> Result<(), std::fmt::Error> {
        // ...
        Ok(())
    }

    fn custom_parse<'src>(
        parser: &mut tir::parse::text::Parser<'src>,
        context: &tir::Context,
    ) -> Result<Box<dyn tir::Operation>, (tir::parse::Span, tir::Error)> {
        // ...
    }
}
```

Generated ISA dialects usually get their custom asm parser/printer from TMDL
instead of hand-written Rust.

## Semantic Expressions

The `sem` section lowers an operation into a small semantic-expression DAG:

```rust
operation! {
    AddIOp {
        name: "addi",
        dialect: "builtin",
        operands: O {
            lhs: "crate::builtin::IntegerType",
            rhs: "crate::builtin::IntegerType",
        },
        results: R {
            result: "crate::builtin::IntegerType",
        },
        sem: "(set result (add lhs rhs))",
    }
}
```

Semantic expressions are used by instruction selection, algebraic rewrites, and
the simulator. If an operation has a result, the generated semantic expression
records the concrete result type from the owning context.

### Rules over an operation

Rewrite rules are written in PDL, one language over two vocabularies told apart
by syntax. `builtin.addi(x, y)` matches your operation by identity;
`#add(x, y)` matches the semantic operator it lowers to. A rule may name both,
which is how a dialect states its bridge to the semantics the e-graph reasons
over:

```
rule add-zero: builtin.addi(x: int<W>, 0) => x;
rule addi-is-add: builtin.addi(x: int<W>, y) <=> #add(x, y) proof trusted;
```

`core/src/passes/instcombine/rules.pdl` holds the peephole rules,
`core/defs/isel.pdl` the target-independent semantic invariants, and each
backend its own rule file. The language is described in
`docs/design/instruction_selection.md`.

## TMDL-Generated Dialects

Target ISAs are usually too large to maintain as hand-written Rust. A TMDL file
can describe:

- register classes and aliases,
- instruction definitions,
- assembly templates,
- instruction behavior,
- scheduling units and machine models.

The backend build script runs `tmdlc --action=emit-rust`, includes the generated
Rust from `OUT_DIR`, and then the handwritten backend module adds the dialect
declaration, special virtual operations, and target-specific lowering passes.

Keep generated code clippy-clean by fixing the generator templates in
`tmdl/src/rustgen.rs`, not by adding warning suppressions around `include!`.

## Checklist

When adding or changing a Rust dialect:

1. Add all operation and type definitions.
2. Register them in the `dialect!` declaration.
3. Implement every listed interface trait.
4. Add custom parse/print only when the generic format is not enough.
5. Add roundtrip tests for types and operations.
6. Run `cargo fmt --check`.
7. Run `cargo clippy --workspace --all-targets -- -D warnings`.
8. Run `cargo test --workspace`.
