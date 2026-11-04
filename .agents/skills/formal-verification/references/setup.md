# Tool setup

First include existing installer directories in this session's PATH, honoring
custom locations. A missing PATH entry does not mean a tool needs reinstalling.
Then check the installation:

```sh
export PATH="${ELAN_HOME:-$HOME/.elan}/bin:${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"
command -v cargo-hax lean lake cargo-kani
cargo hax --version
cargo hax extract --help
cargo kani --version
```

Run the checks relevant to the selected tools individually so a missing command
does not hide the others. `lean` and `lake` may be Elan shims with no global
default. Check their presence from the workspace; check their versions inside
the generated Lean project, where `lean-toolchain` selects the version.

## Hax and Lean

TIR uses the Aeneas-backed `lean` backend. The existing proofs were checked with
`cargo-hax 0.4.2`. Respect a newer repository pin if one has since been introduced.
To reproduce that CLI installation when it is missing:

```sh
cargo install --locked cargo-hax --version 0.4.2
```

This is the published CLI used by the existing TIR proofs. The
[Hax repository's installation instructions](https://github.com/pereq-dev/hax#installation)
also describe source installations for unreleased changes. Do not switch to a
different backend's setup procedure for a Lean target.

Install Elan if its shims are still missing after checking PATH:

```sh
curl -sSf https://elan.lean-lang.org/elan-init.sh | sh -s -- -y --default-toolchain none --no-modify-path
```

[Elan](https://github.com/leanprover/elan#installation) supplies the `lean` and
`lake` shims. Its installer supports keeping shell configuration and the global
default untouched, as implemented in its
[installer](https://github.com/leanprover/elan/blob/master/src/elan-cli/self_update.rs).
Use the selected project's generated `lean-toolchain`; do not set a global Lean
default to make a workspace-level `lean --version` succeed.

Hax resolves Charon/Aeneas from `hax.toml` and downloads its managed binaries on
demand. From the package directory, inspect or prefetch those versions with:

```sh
cargo hax tools show
cargo hax tools install
```

Keep the compatible `[tools]` and `[versions]` values in
[`libs/adt/hax.toml`](../../../../libs/adt/hax.toml) as the starting point for a
new package. Hax generates the Lean toolchain and library requirements from
these settings. See [tool management](https://hax.cryspen.com/manual/tools/).
The host needs Rustup, a C toolchain, Git, and network access for initial setup.
Charon and Kani can use Rust toolchains different from TIR's own nightly.

## Kani

The existing harnesses were checked with Kani 0.68.0. Install the verifier package
and run its separate setup step when missing:

```sh
cargo install --locked kani-verifier --version 0.68.0
cargo kani setup
cargo kani --version
```

The [Kani installation guide](https://model-checking.github.io/kani/install-guide.html)
documents both steps and supported host platforms. `cargo kani setup` downloads
the compiler, solver, and supporting toolchain. A present cargo plugin is not
evidence that setup has completed. If version invocation reports missing setup,
run setup before attempting proofs.

Use the repository's current version requirements if they change. Recheck any
recorded intrinsic limitations when upgrading; a successful installation alone
does not establish that a previously unsupported proof now works.
