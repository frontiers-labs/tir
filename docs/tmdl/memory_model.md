# TMDL Memory Model: Atomics, Reservations, and Fences

This document specifies TMDL's memory-access vocabulary beyond plain
`load`/`store`: memory ordering annotations, load-reserved/store-conditional,
atomic read-modify-write, and fences. It covers the surface syntax, the
semantic-IR encoding, and the semantics each consumer (Rust executor codegen,
SMT-LIB codegen, the simulator) gives to the new constructs. The RISC-V A
extension (`backends/riscv/defs/atomics.tmdl`) is the reference user.

## Scope and goals

- **Target-neutral vocabulary.** Ordering annotations use a generic
  acquire/release model (in the spirit of Sail access kinds and C++
  `memory_order`), not RISC-V aq/rl bits or ARM mnemonic conventions. Each
  target maps its encoding bits onto this vocabulary in instruction behaviors.
- **Instruction-granular atomicity.** An atomic operation is a single
  behavior-level expression, never a multi-statement region. This matches
  Sail's treatment of AMOs as single read-modify-write accesses and is forced
  by both backends (see "Why not an `atomic { }` block" below).
- **Single-hart today, multi-hart-ready IR.** Ordering is carried through the
  IR and recorded by the simulator but is semantically inert while there is
  one hart and SMT models one instruction per transition. Nothing in the
  surface language or IR needs to change to give it meaning later.

## Ordering vocabulary

Orderings are spelled as members of the reserved `Ordering` namespace and have
type `bits<3>`:

| Constant | Code | Meaning |
|---|---|---|
| `Ordering::relaxed` | 0 | no ordering constraint |
| `Ordering::acquire` | 1 | no later access may be reordered before this one |
| `Ordering::release` | 2 | no earlier access may be reordered after this one |
| `Ordering::acq_rel` | 3 | both acquire and release |
| `Ordering::seq_cst` | 4 | single total order across all `seq_cst` accesses |

Target mapping:

| Target encoding | Ordering |
|---|---|
| RISC-V `aq=0 rl=0` | `relaxed` |
| RISC-V `aq=1 rl=0` | `acquire` |
| RISC-V `aq=0 rl=1` | `release` |
| RISC-V `aq=1 rl=1` | `acq_rel` |
| ARM `ldar`/`ldaxr` (future) | `acquire` |
| ARM `stlr`/`stlxr` (future) | `release` |

The `.aqrl` → `acq_rel` mapping is uniform across LR/SC/AMO. RVA20 gives
`lr.aqrl`/`sc.aqrl` sequentially-consistent semantics; revisit this mapping
(possibly `seq_cst` for the `.aqrl` LR/SC forms) when SMT verification against
the Sail model is enabled for the A extension.

## Builtins reference

| Builtin | Value | Kind |
|---|---|---|
| `load(addr, bytes, meta[, ordering])` | `bits<8*bytes>` | expression |
| `store(addr, bytes, value[, ordering])` | — | effect statement |
| `load_reserved(addr, bytes, ordering)` | `bits<8*bytes>` | expression |
| `store_conditional(addr, bytes, value, ordering)` | `bits<1>` | expression |
| `atomic_rmw(op, addr, bytes, value, ordering)` | `bits<8*bytes>` | expression |
| `fence(pred, succ)` | — | effect statement |
| `fence_i()` | — | effect statement |

- `load`/`store` gain an optional trailing ordering argument; omitted means
  `Ordering::relaxed` and is fully backward compatible.
- `load_reserved` reads memory and registers a reservation for the accessed
  address range.
- `store_conditional` writes memory iff a valid reservation covers the exact
  address and size; it evaluates to `1` on success, `0` on failure, and
  consumes the reservation either way. ISA-level result conventions are the
  behavior's job — RISC-V SC writes `rd = 0` on success:

  ```
  rd = if store_conditional(rs1, 4, extract(rs2, 31, 0), Ordering::relaxed) == zext(0b1, 1) {
      zext(0b0, self.XLEN)
  } else {
      zext(0b1, self.XLEN)
  };
  ```

  The `zext(0b1, 1)` — rather than a bare `0b1` — is required: an integer
  literal has type `Integer` and does not unify with the `bits<1>` result of
  `store_conditional`.

- `atomic_rmw` performs one single-copy-atomic read-modify-write and evaluates
  to the **old** memory value. `op` is a bare identifier from the closed set
  `add, swap, xor, and, or, min, max, minu, maxu` (codes 0..8); `min`/`max`
  compare signed at the access width, `minu`/`maxu` unsigned. RISC-V
  `amoadd.w`:

  ```
  rd = sext(atomic_rmw(add, rs1, 4, extract(rs2, 31, 0), Ordering::relaxed), self.XLEN);
  ```

- `fence(pred, succ)` is a data-memory ordering fence. `pred`/`succ` are
  target-defined bit sets carried verbatim (RISC-V: the 4-bit `iorw`
  predecessor/successor sets). `fence_i()` is an instruction-stream fence.
  Both are statements, like `trap`.
- Loads of RISC-V LR (`lr.w`) use `load_reserved` under `try`/`except
  misaligned_load` with cause 4; SC and AMOs use `except misaligned_store`
  with cause 6.

Structural rules enforced by sema:

- At most one `load_reserved`/`store_conditional`/`atomic_rmw` call per
  behavior statement, and it must appear within the right-hand side of a
  single assignment (pure wrapping such as `sext`/`zext`/`extract`/`if` is
  allowed). A bare-statement `store_conditional` with a discarded result is
  also legal.
- `fence`/`fence_i` are valid only in statement position.
- `try`/`except` classification: `misaligned_load` matches `load` and
  `load_reserved`; `misaligned_store` matches `store`, `store_conditional`,
  and `atomic_rmw`. The at-most-one-faulting-access-per-`try` restriction of
  the SMT backend is preserved because each atomic behavior contains exactly
  one access.

## Why not an `atomic { }` block

A region form (`atomic { t = load(...); store(...); }`) was rejected:

1. The SMT backend permits at most one faulting memory access per `try`
   block; a load+store region has two, making `except misaligned_store`
   untranslatable.
2. Sail models AMOs as one read-modify-write access, not two independently
   observable memory effects.

A single-expression `atomic_rmw` avoids both problems and keeps the
value/effect split (old value in a register write, memory update as the
effect) explicit for both backends.

## Static vs dynamic ordering (decision record)

The ordering argument is an ordinary IR expression child, so a *dynamic*
ordering — computed from decoded aq/rl operand bits — is representable
without IR changes. The RISC-V A extension nevertheless instantiates **four
static instruction variants** per mnemonic (`amoadd.w`, `.aq`, `.rl`,
`.aqrl`) via TMDL macros, because:

1. Assembly parser/printer dispatch is keyed on the mnemonic token and asm
   templates are static strings; a suffix that depends on operand values
   cannot be produced or parsed from one definition.
2. The generated decoder guards on literal params, so `AQ`/`RL` params at
   encoding bits 26/25 give exact, conflict-free decoding of all four forms.
3. The macro system exists precisely to absorb this expansion (11 mnemonics
   × 2 widths × 4 orderings from ~60 lines of invocations).

## Semantic IR encoding

New `SymKind`s (crate `tir_symbolic`, re-exported as `tir::sem`):

| Kind | Arity | Children |
|---|---|---|
| `LoadReserved` | 3 | address, bytes, ordering |
| `StoreConditional` | 4 | address, bytes, value, ordering |
| `AtomicRmw` | 5 | op (constant code 0..8), address, bytes, value, ordering |
| `Fence` | 3 | pred, succ, kind (0 = data, 1 = instruction) |

`fence_i()` lowers to `Fence [0, 0, 1]`.

Plain `load`/`store` keep their existing kinds and arities. The ordering is
packed into the previously inert metadata operands: bit 0 retains its old
meaning (load signedness hint / store address space), bits 3:1 carry the
ordering code. Existing three-argument definitions produce the same IR as
before (`Ordering::relaxed` = 0).

Supporting types: `AtomicRmwOp` (op codes plus `apply(old, val)` at the access
width) and `MemOrdering` (codes 0..4), both in `tir::sem`.

The `Memory` trait gains `load_reserved`, `store_conditional`, `atomic_rmw`,
and `fence` methods with composing defaults (plain read; unconditional
success; read-modify-write; no-op), so a memory with no reservation concept
behaves like an uncontended hart and existing implementations keep working.
Accesses wider than 8 bytes are not supported for the atomic builtins.

## Simulator model

The `Executor` holds the single implicit hart's reservation as
`Option<(address, size)>` and implements the `MachineContext` counterparts of
the four `Memory` methods.

Reservation policy (matches Spike on one hart):

- `load_reserved` replaces the reservation with the exact (address, size) of
  the access.
- `store_conditional` succeeds iff the reservation equals the exact
  (address, size); it always clears the reservation, on success and failure.
- Plain stores by the same hart do **not** clear the reservation (permitted
  by RVA; required for differential parity with Spike).
- `atomic_rmw` performs the read-modify-write directly; on a single hart no
  interleaving is possible, so single-copy atomicity is trivially satisfied.
- `fence` does not change architectural state; when tracing, it records a
  `MemAccessKind::Fence` entry so timing models can observe it.

Memory-trace entries carry a `MemAccessKind`
(`Data`/`LoadReserved`/`StoreConditional { success }`/`AtomicRmw`/`Fence`).

Trap causes follow RISC-V: misaligned LR → load-address-misaligned (4);
misaligned SC/AMO → store/AMO-address-misaligned (6). As with plain
loads/stores, the executor itself performs misaligned accesses without
trapping — the trap is modeled in the TMDL behavior and in SMT; differential
tests against Spike must therefore use aligned atomics only.

**Multi-hart seam.** When harts become explicit, the reservation moves into a
per-hart struct together with registers and the PC; `write_memory` must then
clear overlapping reservations of *other* harts, and orderings gain real
semantics in the interleaving model. The `MachineContext` method signatures
are already hart-local and need no change.

## SMT model

The state datatype gains reservation fields, ordered
`registers..., mem, resv, resa, pc`:

```smt
(mem  (Array (_ BitVec 64) (_ BitVec 8)))
(resv Bool)          ; reservation valid
(resa (_ BitVec 64)) ; reserved address
```

with `set_res`/`clear_res` helper constructors. Per-construct transitions:

- **LR**: register write of the loaded value, then `set_res` with the access
  address.
- **SC**: success predicate `(and (resv st) (= (resa st) addr))`; on success
  the memory write applies, and the reservation is cleared on both paths. The
  `bits<1>` value facet of `store_conditional` is the same predicate.
- **AMO**: one transition containing the register write of
  `(read_mem_N st addr)` and the memory write of the combined value
  (`bvadd`, `ite bvslt` for `min`, etc. at the access width).
- **Fence**: identity on the state — architecturally correct while one
  instruction is one transition.

Misalignment integrates with the existing `try`/`except` lowering; the
one-faulting-access restriction holds because atomics are single accesses.

`verify-smt` initially filters A-extension instructions: cross-checking
against Sail requires mapping Sail's reservation register onto `resv`/`resa`
(Sail forks LR/SC paths on its own reservation state). The initial state
constrains `(not (resv st0))`. Enabling A verification is follow-up work.

## Instruction-selection policy

Atomic constructs are excluded from instruction selection and from op-sem
pattern generation: behaviors containing them produce no isel rules and no
`AsSemExpr` impls, and the new kinds are classified impure. Compiling
language-level atomics to these instructions is a separate, target-specific
concern (e.g. pseudo-expansion), out of scope here.

## Limitations and future work

- **Dynamic ordering operands** — decode aq/rl bits into the ordering
  argument to collapse the four static variants; IR-ready, needs surface and
  asm-template work.
- **Keyword fence operands** — `fence` prints/parses its sets numerically
  (`fence 3, 3`); asm templates have no keyword-set form for `iorw` yet. ELF
  decoding of compiler-emitted fences is unaffected.
- **Multi-hart execution** — see the seam note above.
- **Zacas / Zabha** — compare-and-swap and byte/halfword AMOs need either new
  `AtomicRmwOp`s (cas with an extra operand) or a widened builtin.
- **verify-smt for A** — Sail reservation-state mapping.
