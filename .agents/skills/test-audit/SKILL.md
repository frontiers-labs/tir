---
name: test-audit
description: "Invoke whenever writing, changing, reviewing, or sweeping tests. Authoring gate for new tests plus audit workflow for low-value, implementation-coupled, or duplicative tests and the test-only production seams they demand."
---

# Test audit

Three modes, one value bar. Authoring mode gates every new or changed test at
write time. Audit mode runs focused sweeps of tests that re-assert source,
duplicate stronger proof, couple behavior to implementation, or keep test-only
production seams alive. Continue broad audits as separate coherent follow-up
PRs; optimize for confidence, not deletion count. Campaign mode prunes one
whole subsystem's tests, including its cases in shared test crates; before
starting one, read [CAMPAIGN.md](CAMPAIGN.md).

## Authoring gate

Before adding any test, answer four questions; a missing answer means do not
add it yet:

1. What observable behavior, invariant, or independent contract does it protect?
2. What credible regression makes it fail?
3. Why does existing coverage not already catch that failure? Each contract has
   one primary test owner at the strongest boundary; another layer needs its
   own distinct risk, such as a CLI, ABI, or execution failure the owner cannot
   reach. Prefer extending a table-driven case or shared fixture over a
   near-duplicate test; consolidate duplicated setup in the same change.
4. Does it need a production seam (export, flag, wrapper, injection hook) that no
   production caller needs? If yes, move the test to the real boundary instead.

Then check the test against every [junk pattern](#junk-patterns); a match fails
the gate unless the [retention bar](#retention-bar) names the contract it
independently guards. A test that would break under behavior-preserving
refactoring is asserting implementation, not behavior; rewrite it at the owning
boundary before landing it.

Bug regression tests must fail on the pre-fix code for the intended reason and
pass after the owner-boundary repair. A regression test that never demonstrably
failed proves the mock, not the fix. One regression at the owner boundary
covers the bug; do not replay the same scenario at every layer it crosses.

## Junk patterns

The shared checklist for both modes: the authoring gate rejects a new test that
matches one, and audits hunt for existing tests that do.

- assertion-free coverage probes;
- self-comparisons and identity copiers;
- copied fixtures, inventories, manifests, or export lists;
- exact source, import, or string greps;
- private predicate or call-shape tests duplicated at real boundaries;
- duplicate invocations of the same contract;
- backend-local replays of shared helpers;
- tests whose only purpose is preserving test-only exports, globals, or wrappers;
- dead production code whose only callers are tests;
- expected values produced by the helper or renderer under test;
- mocks that implement the asserted behavior, or one identical mock standing in
  for different APIs;
- fixtures that supply the lowering, register assignment, or effect ordering the
  compiler should produce, or assertions against IR the tested path never uses;
- capability tests that restate declared flags instead of exercising the
  compilation or execution behavior the flag promises;
- negative controls that pass for an unrelated reason, such as a denial from a
  different guard or a rejection the production path never reaches;
- names or fixtures that promise more than the input exercises, such as a
  "eliminates the dead store" test that only checks compilation succeeds.

## Value bar

Tests justify their maintenance cost by protecting behavior, a credible
regression, or an independently meaningful contract. In an audit, an existing
test that must change for behavior-preserving source reorganization is suspect,
not automatically deletable; the authoring gate still rejects new ones.

Before judging a candidate, read the complete test and production owner, its
entry point, callers, callees, sibling implementations, overlapping tests, CI
routing, and relevant history. Read root and scoped `AGENTS.md` files first.
When the test claims dependency-backed behavior, inspect the dependency source
or types directly.

## Discovery

Keep discovery read-only and report evidence before editing. For broad scope,
run parallel discovery lanes when available:

- core IR and shared optimizations (`core/`);
- target backends (`backends/`) and their cases in `utils/unit-tests`;
- frontends and DSLs (`fcc/`, `tmdl/`);
- utilities, simulators, scripts, and tooling;
- a cross-cutting pattern sweep.

Use fresh-context agents with bounded assignments and at most three concurrent
subagents, as required by `AGENTS.md`. The main agent owns shared logs.

Outside campaign mode, prefer a few high-confidence candidates over a large
speculative inventory. Hunt for the [junk patterns](#junk-patterns).

## Retention bar

Keep a test when it independently enforces a public API, CLI, IR verifier, pass
semantics, ISA encoding, ABI, effect ordering, floating-point semantics,
protocol, configuration, migration, storage, security, platform, default,
generated cross-language, package, release, or architecture contract. Also keep:

- call ordering when order is observable behavior;
- regressions with a credible failure mode;
- source inspection when it is the cheapest independent guard: it fails when
  the contract changes (the user-facing key, byte, or path) and survives an
  identifier-only refactor;
- a retained test that fails on the baseline: treat it as a possible product
  bug, reproduce it, and repair the owner rather than deleting it.

LIT output checks, property tests, compile-fail tests, and formal verification
can independently enforce these contracts. Exact emitted bytes or diagnostic
codes are not source-grep tests. Generated snapshots still need review against
the intended behavior; regeneration alone does not establish correctness.

Static or slow is not a deletion reason. A test that resembles implementation
may still be the independent contract; prove otherwise before removing it.

## Candidate evidence

Record every field below before editing. A missing field means the candidate is
not ready for deletion:

- exact test name and location;
- what failure it can actually detect;
- non-test callers of the covered production or support seam;
- stronger remaining owner-boundary proof, or why no proof is needed;
- relevant history and the reason the test or seam exists;
- production or test-support deletion unlocked;
- risk and the focused validation command.

## Edit shape

Choose one coherent owner-boundary batch. Delete obsolete test-only exports,
globals, wrappers, and dead production paths instead of preserving aliases.
Move retained regressions to their canonical owners. Consolidate repeated
package or dependency assertions into one generic contract.

Prefer net-negative production LOC. Do not add replacement tests that restate
the same implementation, and do not convert uncertain candidates into cleanup
to increase deletion counts.

## Validation

Never edit source or tests while their validation is running in the checkout.
Follow `docs/dev_guide.md` and the applicable `AGENTS.md` quality gates. Prefer
LIT checks for behavior observed through compiler tools. Heavy-crate public-API
unit tests live in `utils/unit-tests`; light utilities keep tests in-crate.
Never expose private APIs just to make tests reachable.

1. Build compiler tools with `cargo build`, then run the smallest owner and
   sibling tests. Use `LIT_FILTER='<path-regex>' cargo xtask test -- -E 'binary(lit)'`,
   `cargo test -p tir-unit-tests <filter>`, or the owning utility package. Confirm
   that the intended cases actually ran.
2. For removed source greps or plan assertions, run the executable script or
   dry-run that owns the real contract.
3. Run `cargo fmt --check`, then `git diff --check`. Regenerate intentional
   golden-output changes with `utils/scripts/update_checks.py` as documented
   in the developer guide, and inspect the resulting assertions.
4. Run the required project gates: `cargo build`,
   `cargo clippy --workspace --all-targets --no-deps -- -D warnings`, and
   `cargo xtask test`. Run
   `cargo xtask test --profile ci --torture` for applicable core IR or FCC changes and the
   required benchmark comparisons. For execution claims, run the compiled
   program and check its result. Missing infrastructure or skipped cases are
   gaps, not passes.
5. Inspect `git diff --numstat`; report production/tooling separately from
   tests and test support.
6. After final audit edits, have an independent agent review the full diff for
   correctness, production simplifications, lost coverage, and project-rule
   compliance. Resolve its findings before claiming completion.
