# TIR Haskell bindings (proof of concept)

FFI bindings to TIR over the generic C ABI (`tir-capi`). Because the C ABI is
generic over the uniform IR, a handful of `foreign import ccall` declarations in
`src/Tir.hs` drive every dialect — no per-op code. This PoC covers the generic
verbs: create a context, parse a module, run a pass pipeline, print, and read the
operation schema.

## Build & test

Run from the repository root with `ghc` on `PATH`:

```sh
cargo build -p tir-capi
mkdir -p target/haskell
ghc -O0 -outputdir target/haskell -iutils/haskell/src utils/haskell/test/Main.hs \
    -L target/debug -ltir_capi -optl-Wl,-rpath,$PWD/target/debug \
    -o target/haskell/tir_hs_smoke
target/haskell/tir_hs_smoke
```

That builds `libtir_capi`, then compiles and runs `test/Main.hs` linked against
it (the smoke test parses a module, runs `instcombine`, and checks the output).

To use the module directly:

```sh
cargo build -p tir-capi
ghc -iutils/haskell/src your_program.hs \
    -L target/debug -ltir_capi -optl-Wl,-rpath,$PWD/target/debug
```

## Example

```haskell
import Tir

main = withContext $ \ctx -> do
  m <- parseModule ctx "module { func @f() { return } module_end }"
  runPipeline ctx m "func.func(instcombine)"
  putStrLn =<< opToString ctx m
```
