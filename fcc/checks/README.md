# fcc checks

Run via the workspace LIT driver: `cargo test -p tir-lit --test lit`.

Golden tests (`Preprocessor`, `Lexer`, `Ast`) are regenerated with
`./utils/scripts/update_checks.py fcc`; the `Codegen` tests are authored by
hand.
