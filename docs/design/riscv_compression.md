# RISC-V Compressed Instructions

The RISC-V C extension provides 16-bit encodings for the most common
instructions. Every compressed instruction is a strict narrowing of a base
instruction: same semantics, but a tied destination (`c.add rd, rs2` means
`add rd, rd, rs2`), a 3-bit register field reaching only x8..x15/f8..f15, or a
small scaled immediate. `defs/compressed.tmdl` models the full RV32C/RV64C
integer set plus the Zcd/Zcf float loads and stores; the TMDL encoding width
is data-driven (the highest encoding bit defines `width_bytes`), so 2-byte
instructions flow through assembly, object emission, and simulation without
special cases.

## Where compression happens

Compressed instructions are excluded from instruction selection
(`COMPRESSED_FEATURES` in `lib.rs`) and applied by a post-RA rewrite
(`compress.rs`, registered in `finalize_lowerings` when `Feature::C` is
enabled). This placement is the core design decision:

- **Selecting compressed forms early would constrain the allocator.** A
  `c.and` pattern would force its virtual registers into the 8-register GPRC
  class and tie the destination to a source. Both restrictions raise register
  pressure and copy counts — a real performance cost — to save 2 bytes that a
  late rewrite gets for free whenever allocation happens to satisfy them.
- **Post-RA, compression is a pure win.** Registers and immediates are known,
  so the rewrite is a per-instruction pattern check: `add rd, rd, rs2` becomes
  `c.add`; `lw` from sp becomes `c.lwsp`; the return `jalr x0, 0(x1)` becomes
  `c.jr ra`. Nothing about scheduling or allocation changes, only encoding
  size (and with it fetch bandwidth and icache footprint).

The 3-bit register fields need no encoder remapping: GPRC keeps the
architectural indices 8..15 (`file = GPR` aliases the classes index-for-index)
and the encoder's field-width mask keeps exactly the low three bits, which are
0..7 for x8..x15 by construction.

## What is not compressed, and why

PC-relative control flow (`c.j`/`c.jal`/`c.beqz`/`c.bnez`) is modeled — it
assembles, encodes, relocates (`R_RISCV_RVC_JUMP`/`R_RISCV_RVC_BRANCH`), and
simulates — but codegen never narrows a `jal` or a conditional branch into it.
Their targets are fixups resolved at object emission, and the binary writer
has no branch relaxation: a compressed conditional branch reaches ±256B and a
compressed jump ±2KB, so an out-of-range target would be a hard emission error
rather than a fallback to the 4-byte form. Since taken-branch cost dominates
their encoding size, this loses little.

## Getting more compression out of the allocator

The rewrite compresses whatever the allocator hands it, and the allocator is
currently compression-blind. Ordered by payoff, the next steps are:

1. **Bias the GPR allocation order toward x8..x15 when C is enabled.** The
   3-bit-field forms (`c.lw`, `c.sw`, `c.and`, `c.beqz`, ...) only fire when
   both registers land in x8..x15. Today's order prefers t0..t2 (x5..x7)
   first, which can never compress into those forms. Preferring
   a0..a5/s0/s1 costs nothing at allocation time and directly raises the hit
   rate. This needs the allocation order to become feature-dependent
   (`RegisterInfo` is currently a static table).
2. **Prefer tied-operand assignments.** Two-address forms (`c.add`,
   `c.andi`, `c.slli`, ...) need `rd == rs1`. A same-register preference
   between an op's destination and its first source — the classic
   move-coalescing hint, absent from the PBQP costs today — would convert
   many 4-byte ALU ops. The PBQP formulation makes this a natural extension:
   a small cost discount on matching colors, never a constraint.
3. **Branch relaxation in the binary writer.** Emit compressed branches
   optimistically and widen the encoding when the resolved offset does not
   fit. This unlocks `c.beqz`/`c.j` for codegen (the largest remaining
   category) and also lifts the existing hard ±4KB limit on base branches.
   Relaxation moves later offsets, so it must iterate to a fixed point.
4. **Frame layout aware of `c.lwsp`/`c.sdsp` ranges.** Spill slots and
   callee-save areas placed within the sp-relative compressed offset ranges
   (0..252/0..504 bytes) keep spill traffic 2-byte. Only matters once frames
   grow past those ranges.

Each step is independent and none changes instruction semantics: the TMDL
definitions above stay the single source of truth for encodings and behavior.
