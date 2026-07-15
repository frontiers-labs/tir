# Checked AST JSON

`tmdlc` exposes its checked abstract syntax tree as a versioned JSON contract:

```console
tmdlc --action=emit-ast-json --output=ast.json input.tmdl
```

The output contains the input files in command-line order and their declarations
in source order. It represents the same stage used by the other checked-AST
actions: macros and register-class inheritance have been processed, semantic and
type checks have passed, and template inheritance has not been normalized into
each instruction.

The root `version` identifies the JSON contract. Version 1 uses flat records with
a snake-case `kind` field. Optional values, empty collections, and false defaults
are omitted; consumers should interpret their absence as `None`, an empty
collection, or `false`, respectively. Numeric literals remain strings so their
source radix and spelling are preserved. Source spans are not exported.

## Schema

The complete field and variant reference is the generated
[JSON Schema](./ast-v1.schema.json). Generate the schema supported by the current
compiler with:

```console
tmdlc --action=emit-ast-json-schema --output=-
```

The schema action accepts no TMDL inputs. To update the committed schema after an
intentional contract change, run:

```console
tmdlc --action=emit-ast-json-schema --output=docs/tmdl/ast-v1.schema.json
```

The schema is generated from the same Rust export types as the JSON output and a
test rejects drift between the command and the committed file. Any incompatible
field, variant, or meaning change requires a new version and schema file; retain
older schema files for existing consumers.
