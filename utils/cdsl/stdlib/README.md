# Embedded modules

`cdslc` and `cdsl!` embed these CDSL files through their shared compiler library.
No installed library or runtime search path is needed. Both sources use the
ordinary parser, resolver, termination checks and typechecker.

```
use std::*;
use std::nat::Nat as Count;
pub use std::list::List as Sequence;
let zero: Count = Count::Zero;
```

Imports bind canonical declaration identities. Wildcards bind only the module's
public exports. Public re-exports retain the original identities; private helpers
remain in checked output for public definitions that depend on them. Conflicting
names and cyclic imports are errors. The `std` root always denotes bundled source.

`Nat` is a source-defined inductive type with `Zero` and `Succ`, not the inherited
compiler numeric literal type. `List<A>` is a source-defined generic inductive
family. `Equal<A, B>` witnesses equality of types, with `Refl<T>: Equal<T, T>`.
The `ir` module declares `Ty`, its constructors, `El` and `Value`. The `bits`
module declares numeric operation signatures. Both use the ordinary source
parser and checker. IR constructors no longer have a separate Rust signature
catalog. Source records and indexed enums also define operation signatures,
resource capabilities, graph contexts, region bindings, and interface evidence.

File compilation resolves `mod name;` relative to its declaring source file:
`lib.cdsl` and `mod.cdsl` use their directory; other files use a sibling directory
named after the file stem. `name.cdsl` and `name/mod.cdsl` are alternatives;
providing both is an error. Inline compilation supports embedded modules and
rejects file module declarations because it has no caller-owned file root.

The checked output is a diagnostic source artifact. These modules do not install
native TIR registrations or execute the described IR or relational schedules.
