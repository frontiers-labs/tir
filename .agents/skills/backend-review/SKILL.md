---
name: backend-review
description: Review changes to TIR's backend systems. Always use this skill before making a commit that contains changes to backends/ or core/src/backend/
---

# Non-negotiables

If any of the following conditions is not met, the entire patch is rejected on the design
level. There is ZERO TOLERANCE to violation below rules. No further analysis is needed
to abandon the change set.

- Any change to TMDL lexer, parser or AST. Unless the initial user request specifically
  indicated a need for a change in TMDL syntax, no modification of those parts is allowed.
  There can be no reason to allow merge of such a patch.
- Any form of non-formally verified operation lowering or selection. This includes but not
  limited to: adding hooks to pre_ra_lowerings, introducing "pseudo instructions",
  "virtual operations", custom hand-written  instruction selection rules. If selection does
  not go through e-graph, there's nothing to salvage in the patch. It must be fully rejected.
  Expanding selection behavior is only allowed by improving the formal theory.
- Dynamically sized encodings. One encoding == one instruction. If encoding somehow requires
  dynamic size, that is not a single instruction => patch has a design flow => immediate reject.
- Any extension of symbolic language, unless the behavior can not be provably expressed via
  a combination of existing operations. "Or-combine", "vector load" and other compound operations
  must be immediately rejected.

# Good patches

Below examples show what a good patch usually looks like.

- A set of LIT-based checks. String or integer comparisons in Rust unit-tests are an antipattern.
  Either property testing or LIT snapshots must be used to test behavior of these components.
- All changes must be target-independent. There can be no language-specific constructs or hooks
  in the API.
- Minimally viable set of changes to get the job done. No customization or extensibility beyond
  what was requested in the initial prompt.
- Anything that changes isel logic must also update isel documentation in docs/.
- No ISA-specific or ABI-specific information is exposed to builtin dialect or core TIR. This
  stays strictly in the backends OR in the frontend if required by language semantics.
