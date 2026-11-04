# Add a Kani target

## Wire native harnesses into the crate

Keep proofs beside their production owner. A private function can be reached
from a child module without exporting it:

```rust
#[cfg(kani)]
mod proofs;
```

Put that module at the Rust-resolved path and call the actual function through
`super`. Follow [DenseTrie's module](../../../../libs/adt/src/dense_trie/proofs.rs).
Kani supplies its proof APIs during verification; this pattern needs no normal
`kani` dependency and no `#[cfg(test)]` wrapper. Reuse the workspace's existing
`cfg(kani)` check-cfg registration.

For a new package, opt in to the common runner's Kani stage with native Cargo
metadata:

```toml
[package.metadata.kani.flags]
output-format = "terse"
```

Additional supported flags, such as `jobs`, belong here when useful. Match their
value format to the existing [ADT manifest](../../../../libs/adt/Cargo.toml).
Do not put a development-only harness filter in the package-wide configuration.
This metadata also enables discovery for a Kani-only package; no `hax.toml` or
dummy Lean scenario is needed.

## State a symbolic property

Add `#[kani::proof]` harnesses. Generate arbitrary inputs with `kani::any()` and
use `kani::assume` only for genuine preconditions or explicit dependency
contracts. Preserve full value widths. Assertions must express the independent
contract, not compare the implementation with a copied version of itself. See
[symbolic inputs](https://model-checking.github.io/kani/reference/arbitrary.html).

For a fixed-width helper, cover its entire natural domain. DenseTrie's window
proof uses arbitrary 64-cell `u32` storage, a symbolic valid prefix length and
query, and unconstrained padding. Its partition assumptions cover every sorted
prefix without requiring a small set of sample values.

Set `#[kani::unwind(N)]` to cover all relevant harness and callee loops. The final
termination check can require one more step than the maximum body iterations;
the DenseTrie harness uses 65 for its 64-cell domain. Keep unwinding assertions
and default safety, overflow, and undefined-function checks enabled. Consult
[loop unwinding](https://model-checking.github.io/kani/tutorial-loop-unwinding.html).
An arbitrary input-size cap proves only that bounded domain; do not describe it
as an arbitrary-size collection proof.

Use `kani::cover!` for meaningful boundary and result classes to detect vacuous
preconditions, including the largest size and empty/nonempty results where
applicable. Check that those covers are reachable. Coverage witnesses accompany
the asserted property; they do not replace it.

## Handle architecture and library boundaries

Instantiate production SIMD implementations individually. State ISA-availability
assumptions when using `assume_supported`, and guard architecture-specific
harnesses with the appropriate target cfg. A scalar fallback proof establishes
the fallback implementation only.

The existing Kani 0.68 suite proves fallback, SSE2, and AVX-512 windows. SSE4.2
and AVX2 unsigned comparisons reach `simd_select` through stdarch's unsigned
minimum operation, which that installed verifier rejects. This is a recorded
tool-support gap, not a counterexample. Reproduce the diagnostic on the current
tool before carrying the exclusion forward. Do not disable checks or stub the
target to turn an unsupported path into a passing proof.

If an omitted dependency needs a contract or supported stub, keep that boundary
explicit and disclose its assumptions. This does not require proving the whole
dependency stack. The production function whose correctness is claimed must
still execute in the proof.

## Run and inspect

Use native focused commands during authoring, replacing the package and harness:

```sh
cargo kani -p my-package --harness my_module::proofs::contract --exact
cargo kani -p my-package
```

Check successful harness counts, failed checks, unwinding results, and covers.
Then run `cargo xtask formal-test -p my-package`. Kani-only packages require
only Kani; mixed packages also check their Lean scenarios. Run the unfiltered
`cargo xtask formal-test` to discover every configured workspace package.

Complete the actual-source mutation check and restored green run. Keep native
solver output under ignored `target/`. Report input bounds, preconditions,
assumed dependency behavior, selected architectures, and unsupported paths.
