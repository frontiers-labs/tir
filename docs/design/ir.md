# The Core IR

Status: approved design, revision 1. Implementation is staged; where this
document and the code disagree, this document describes the target and the
code describes the past.

Normative external reference for the middle-end semantics: the RVSDG paper
(Reissmann, Reusch, Bahmann, Själander — arXiv 1912.05036). This document
uses its vocabulary (γ, θ, state edges) for *explanation only*; no Greek
appears in code, and the core defines no node kinds for these concepts.

## 1. One IR, many forms

TIR has exactly one IR infrastructure: a `Context` holding dialect-defined
operations with operands, results, attributes, and regions, whose semantics
are carried by interfaces. There is no second graph, no shadow arena, no
parallel node universe. What changes over the course of compilation is not
the infrastructure but the *form* — which dialects appear and which
structural conventions hold:

```
source ─▶ AST ─▶ cir (frontend CF)  ─▶ scf-as-RVSDG (middle end)
                                          │
                                          ▼
object ◀─ emission ◀─ machine CFG ◀─ selection from the e-graph view
                       (order derived by scheduling)
```

- **cir** (and any future frontend dialect) owns frontend control flow, and
  may emit its own structured ops (preferred where the AST is structured —
  they preserve loop bounds for a mid-end pass to read) or an arbitrary CFG. Unstructured
  control flow is first-class *input*: the `restructure-nodes` pass (§5.2)
  totally converts any CFG region — irreducible included — to one unordered
  region of structured ops. Structure is a guarantee the compiler provides,
  never a restriction on input.
- **scf** is the middle-end form: structured regions whose conditional and
  loop semantics are read through interfaces. This form is RVSDG in the
  paper's sense — an acyclic, region-hierarchical, state-threaded program —
  without any dedicated RVSDG ops (§5).
- **machine CFG** exists only after destruction, which happens inside
  emission at the end of instruction selection. Within a machine block, the
  final instruction order is *derived* by the scheduler from dependence and
  the machine model, not inherited from any earlier order (§9).

Three rules are binding for everything below:

1. **Interfaces only.** Core and optimizer code never matches
   `(dialect, name)`. A pass that lowers a dialect may know that dialect's
   ops; nothing else may. The core holds no closed enums of op kinds and no
   per-op knowledge.
2. **No raise/lower round-trips.** The IR in front of a pass is the IR.
   Form transitions are ordinary forward-only conversion passes with LIT
   coverage. Nothing converts the program into a private representation and
   back as a hidden implementation detail.
3. **No parallel paths.** A replacement is finished when the replaced code is
   deleted. Bring-up flags may exist while a change is in flight and must be
   gone when it lands.

## 2. The green core

The green core is the persistent ground truth: the `Context` and the entities
it owns. "Green" means *immutable in place*: entities are edited only by
building replacements and swapping them in through the tree-edit API, which
stamps versions along the edited spine. Everything derived — use lists,
dominance, e-graphs — lives outside the green core as red views (§7).

### 2.1 Entities and identity

| Entity | Id | Contents |
|---|---|---|
| Operation (`OpInstance`) | `OpId` (dense `u32`) | `(dialect, name)` identity, operands `Vec<ValueId>`, results `Vec<ValueId>`, regions `Vec<RegionId>`, attributes |
| Value | `ValueId` | type (`TypeId`), defining op (`None` for block arguments) |
| Block | `BlockId` | argument values, ordered op list |
| Region | `RegionId` | ordered block list, parent op |
| Type | `TypeId` | interned, hash-consed; identity is structural equality |

Conventions:

- Ids are dense, monotonically assigned, and **never recycled**. An erased
  entity's slot empties; the id stays dead forever. Any walk over a
  previously captured id set must tolerate dead ids.
- Op identity is the `(dialect: &'static str, name: &'static str)` pair.
  Typed op structs (`AddIOp`, `ForOp`, …) are zero-cost newtype wrappers over
  `OpHandle`, the `(context, id)` pair naming an op; `op.is::<ForOp>()`
  compares the identity pair. The
  `OperationName`/`DialectName` newtypes exist so string comparison against
  op names does not compile.
- A block argument is a `Value` with `defining_op == None`. There is no
  separate block-argument type.

### 2.2 Storage and the mutability discipline

Operations, values, blocks and regions each live in a dense `Vec` inside
`Context`, with a slot table mapping the entity's id to its position. `OpInstance`,
`Block`, and `Region` are plain structs with **no interior mutability** — no
per-field locks, no cells. Mutation happens in exactly one place: inside the
Context's write lock, invoked only by the tree-edit API. Consequences developers
rely on:

- An `OpHandle`, `BlockHandle` or `RegionHandle` reads its entity **as it
  stands**: each accessor takes the context lock and copies out. It is not a
  snapshot, and a handle to an erased entity panics rather than answering — read
  what you need before erasing.
- The green core stores **no derived data**. In particular it does not
  maintain use lists (the `DefUse` view owns those, §7.3), and blocks do not
  store successor/predecessor lists (CFG edges are read from the
  `Terminator` interface where a CFG exists at all).

The storage layout is an implementation detail behind the tree-edit and
accessor API. A data-oriented SoA layout may replace what is left of the `Arc`
slabs without changing anything in this document except this sentence.

### 2.3 Versions

Every operation carries a `u32` version stamp. Every tree edit bumps the
version of each op along the spine from the edit site to the root — bumping
the *root* is what lets function- and module-keyed caches detect changes made
anywhere inside nested regions. The pair `(OpId, version)` is the universal
cache key:

- Analysis results are cached under `(OpId, version)`. Invalidation is a
  staleness comparison, not an event: a cached result whose key no longer
  matches the entity's current version is simply not a hit. There is no
  "invalidate everything after every pass".
- Post-pass IR verification (debug builds, `TIR_VERIFY_IR` override) walks
  only dirtied spines.

### 2.4 The tree-edit API

One mutation surface. All of these run under the context write lock, bump
spines, and maintain nothing but the green truth:

| Edit | Notes |
|---|---|
| `create_op` / builders | ops register fully formed; results created with the op |
| `erase_op`, `replace_op` | replace RAUWs results when arities match |
| `insert_op_before` / `insert_op_after` | positional insertion in a block |
| `set_op_operand(s)`, `set_op_attributes` | operand/attribute rewiring |
| `replace_value_uses` (RAUW) | consults the `DefUse` view for use sites |
| `append_block_argument`, `split_block`, `clone_op`, `clone_region`, `splice_region` | block/region surgery; block ids are stable across argument edits |
| **port edit** | grow/shrink an op's results, its regions' arguments, and the corresponding yields *in one edit* |
| `replace_region_contents(op, staged)` | the atomic commit: §2.5 |

The **port-edit primitive** deserves emphasis: any transform that threads a
new value through a structured op (a promoted scalar, a state chain, a
hoisted loop-carried value) needs to extend the op's results, the region's
arguments, and the terminator's operands coherently. That is one primitive
here, not a per-pass idiom. Consumers: `restructure-nodes` drawing the chain
(§6), `promote-nodes` (§5.3), e-graph commit (§7.2), and any future structural
transform (unrolling, inlining, SROA).

There is no separate `IRBuilder` and no free-floating mutation helpers; the
insertion-point conveniences are methods on the same API.

### 2.5 Staged regions and atomic commit

A red view that wants to rewrite a region does not edit op by op. It builds a
**staged region** — a detached builder producing new ops against the same
Context types/interners — and lands it with `replace_region_contents`: one
edit, one spine bump, old contents unreachable. Discarding an exploration is
dropping the builder; the green core never observes it. This is the
commit-or-discard contract every view is written against (§7.1), and it is
what makes speculative optimization and parallel exploration safe.

## 3. Types and attributes

Types are interned and hash-consed in the Context; `TypeId` equality is
structural equality. Dialects define types with `#[derive(TirType)]` and
register them alongside their ops. Verification uses `TypeConstraint`
predicates on operand/result declarations.

One kind of value is not typed at all: a **dependency**, the memory-ordering
edge of §6. A dependency carries no data, so it has no type to spell; what
marks it is its position. Every operand, result, block-argument and region-port
list is two partitions, the values first and the dependencies trailing, and
the counts live on the operation, block or region (`value_operands()` /
`dep_operands()` and their kin). The text puts the dependencies after a `|` on
either side, without types: `%v | %s1 = ptr.load %p | %s0 : !i32`. Dependencies
flow through region ports, loop carries and yields like any other value, with
no special cases in scf or the verifier beyond the memory discipline (§6.4) and
the partition check that keeps a dependency out of a value slot.

Attributes are `(name, AttributeValue)` pairs on ops; `AttributeValue` is the
closed data enum (ints, strings, arrays, dicts, registers, types, blocks).
Attributes carry *data*, never semantics — semantics is interfaces.

## 4. Dialects and interfaces

### 4.1 Defining ops

Dialects are declared with the `dialect!`/`operation!` macros: operand and
result specs (with `?` optional and `*` variadic markers), attribute specs,
region specs, verifier hooks, printer/parser (generic or custom), the
interface list, and optionally `sem:` — an executable semantic expression
from which constant folding and e-graph expansion derive automatically.

Registration is per-Context: `register_dialect::<D>()` installs op
constructors, parsers, type parsers, and each op's interface converters.

### 4.2 The interface mechanism

An interface is a Rust trait implemented by concrete op types and registered
so it can be queried dynamically: `op.as_interface::<dyn LoopLike>()`,
`op.has_interface::<dyn Conditional>()`. Interfaces may carry
`verify_interface` hooks that the generated verifier runs. This is the
**only** extension mechanism. Adding behavior to the compiler means defining
or implementing an interface; it never means teaching core code about an op.

### 4.3 Interface catalog (core)

| Interface | Contract |
|---|---|
| `Symbol` | named module-level entity; signature, visibility, is-definition |
| `Conditional` | n-ary decision: deciding operand, k regions, per-region yields aligned with results. `scf.if` (2 regions), `scf.switch` (k regions) |
| `GuardedLoop` | conditional-execution reading of a loop: `entry_guard() -> EntryGuard` states the zero-trip guard **structurally** — `Less` (ordering + two existing operands) for a counted loop, `Region` (condition region, its arguments aligned with `inits()`, the condition value) for a general one, `AlwaysTaken` for a tail-controlled one — because the guard is not a materialized `Value`. A sibling of `Conditional`, not an extension: a loop has no `decision()` value to hand back and no arm-aligned yields, so widening `Conditional` would either force a comparison into the IR or hollow out its contract for `scf.if`/`scf.switch` |
| `LoopLike` | n-ary iteration: `inits() / carried_args() / latched() / finals()`, arity-verified. Counted loops additionally expose bounds/step untouched |
| `TokenScope` | regions whose entry args are non-forwarding control tokens |
| `Terminator`, `BranchTerminator` | CFG structure where a CFG exists (frontend dialects, boundary input, machine IR): successors and per-edge operands |
| `BranchGuard` | guarded successors of a conditional terminator; consumed by `restructure-nodes` at the CFG boundary |
| `MemoryRead` / `MemoryWrite` | location, value, **and state accessors**: the state operand read, and (for writes) the state result produced (§6) |
| `PromotableAllocation` | the value naming an allocation eligible for chain-splitting |
| `ConstantLike`, `ConstantFold`, `Commutative`, `IntegerArithmetic`, `SameOperandType`, `OpCost` | value semantics for folding, e-graph seeding, and extraction cost |
| `MachineInstruction` | machine-op marking; register slots are `InstrInfo::regs` |

Interfaces whose consumers disappear are deleted; an orphaned interface is a
bug, not a reserve.

## 5. Control flow in the middle-end form

### 5.1 scf as RVSDG, without canonical ops

The middle end has three structured control ops and needs no more:

- `scf.if` — boolean two-region `Conditional`.
- `scf.switch` — integer-predicate k-region `Conditional`; symmetric
  split/join, no fallthrough between arms.
- `scf.for` / `scf.while` — loops, **kept in their source-shaped form**.

There is deliberately no canonical tail-controlled loop op and no
loop-rotation pass. A head-controlled loop is semantically γ∘θ — "if the
guard holds, a do-while" — and that decomposition lives in the e-graph
view's *terms*, not in the IR: the seeder reads `LoopLike` for the iteration
and `GuardedLoop` for the zero-trip condition, and seeds each loop port
as `If(guard₀, Theta(init, latch), init)` (§7.2). Rewriting the IR to that
shape would buy nothing the view doesn't already have, would duplicate the
condition region textually, and would destroy the counted-loop structure
(`lb`/`ub`/`step`) that iteration-space and affine analysis consume.

Corollaries developers should internalize:

- Loop analyses read `LoopLike` + guard; they never pattern-match `scf.for`.
- A frontend dialect may implement these interfaces on its own ops and get
  the entire optimizer for free.
- Multi-block regions and branch terminators do not occur in the middle-end
  form. `mem2reg`-era dominance machinery does not exist here; nothing in
  the mid-end computes a dominator tree (§8).

### 5.2 Arbitrary CFG: total restructuring

The middle end is structured, period — and that is a *conversion guarantee*,
not an input restriction. The `restructure-nodes` pass implements
Bahmann-Reissmann control-flow restructuring over any CFG region, read
through `Terminator`/`BranchTerminator`/`BranchGuard` (dialect-agnostic), and
emits one unordered region of `scf.for`, `scf.switch` and `scf.loop`, drawing
the memory chain (§6) while the block order is still there to read:

1. **Loop restructuring**: strongly connected components become
   single-entry/single-exit tail-controlled loops; multiple entries and
   exits are funneled through dispatch predicates carried as loop values,
   dispatched with `scf.switch`. Irreducible graphs are handled the same
   way — no node cloning, linear size growth.
2. **Branch restructuring**: the remaining acyclic graph becomes nested
   conditional trees; continuation points that several paths share are
   selected by predicate values rather than duplicated.

Consumers: fcc emits its loops as `cir` loop ops, which the `raise-loops`
pass turns into `scf.for` where the counted shape is provable and into the
same blocks and branches otherwise. Flat CFG is what a whole function gets
when it holds a `goto` or a label, or a `return` under a loop — both name
edges a loop region cannot carry — and what every refused loop gets;
`restructure-nodes` raises them all, and there are no refused shapes.
Hand-written CFG-form `.tir` and tir-jit input take the same path. The backend contract
is *unordered regions in, machine CFG out*; the machine CFG reappears only
when `destructure` lowers the region to blocks, inside selection and for
SPIR-V export, placing each op by the cone that demands it.

Correctness is pinned two ways: LIT snapshots for the canonical hard shapes
(irreducible double-entry loops, multi-exit loops, jumps into loop bodies,
goto-into-switch), and a fuzz harness comparing execution of restructured
versus original CFGs through the JIT.

### 5.3 Demand annotation: the values on the ports

Restructuring gives control structure over memory; construction is not done
until the *values* are on ports. The `promote-nodes` pass is that last step,
and it is construction, not optimization: for every stack slot whose address never
leaves the function, whose accesses all name it whole and agree on one type,
the region that reads it takes its value as an argument, the region that
writes it yields it, and a loop that does either carries it. The allocation
goes with the last access.

The walk needs no dominance and places no φ — the chain the converter drew
says which write a read sees — and it grows a port only where the value is
demanded: a loop the walk leaves through its dependency port carries the
value, a gate it leaves through its dependency result joins what each arm
leaves. What stays a slot — an escaping address, arithmetic reaching part of
it, accesses disagreeing on a type, an access off the chain — keeps every
access and stays on the chain as the memory it is.

## 6. Memory: explicit state

Memory identity and ordering are def-use edges over dependency values, in the
IR itself. No pass recomputes chains; no seeder invents serial numbers. The
edges are the *whole* memory dependence relation of the middle-end: order is
derived from them, not the other way round (§6.3).

### 6.1 Ports

| Op | Dependency ports |
|---|---|
| `ptr.alloca` | *produces* the initial state of its slot's chain (alongside the pointer) |
| `ptr.load` | *takes* a dependency and produces one: the memory it observed, which the join closing its fork names |
| `ptr.store`, `ptr.memset` | take one, produce one |
| `ptr.memcpy`, `func.call` | take and produce the one state every chain they may touch was merged into |
| `func.return` | optional dependency operand: every chain the caller can reach, merged |
| `state.entry_state` | produces one chain's initial state at region entry, one op per chain: `\| %s = state.entry_state` |
| `state.join` | takes any number of dependencies, produces the memory they merge into |
| `state.split` | takes one dependency, produces one name per chain carrying on from it |

These ports are the dependency partitions of §2: a pass grows them with
`grow_dep_port`, `append_dep_operand` and `append_dep_result`, and the value
accessors stop at the partition. No chain enters a function *signature*:
a call's arguments must be exactly what the callee's `!fn` type takes, and ABI
lowering maps region arguments to registers — a dependency parameter would
break both. Hence the entry-state op and the return's dependency operand.

The chains survive the backend boundary. A machine opcode whose `InstrInfo`
effects touch memory declares `state: "in_out"` and carries the chain of the
access it was selected for; a virtual call carries the one every chain it may
touch was merged into; a frame slot the allocator makes is a memory of its own,
rooted at an `entry_state` of its own and threaded with the same fork of reloads
and join before the next store. Memory order is an explicit def-use edge from
construction to encoding, and §6.4's discipline is checked on both sides of
selection — `observes_only` reads a machine instruction's declared effects where
a mid-end op has the memory interfaces instead, and an instruction that writes
nothing observes whatever state it names, a load and a branch alike. Register
allocation is the one thing blind to all of it: a dependency lives in no
register, and the register slots only ever name the value partitions, so
liveness and colouring never see one (`son-backend` B2).

### 6.2 Chains

`restructure-nodes` draws the chains as it converts the CFG (§5.2), one chain
over every access, and `verify-deps` checks after every later pass that the
chain is still whole rather than drawing it again. Splitting the chain per
object is what `AliasFacts` and the shared escape classifier (also the gate for
promotion, §5.3) are for:

- **One chain per object.** Every base object the facts tell apart from all the
  others the function names — a global, a parameter, a stack slot — is a memory
  of its own, threaded through every access to it and flowing through structured
  ops as ordinary carried/yielded dependencies.
- A pointer the facts cannot read back may name any object they cannot rule it
  out of. Where a function holds such an access, only the slots no such pointer
  can reach keep chains of their own; every other object shares the
  **conservative chain**, and so does the unresolved access.
- Two parameters may name one memory, so both sit on the conservative chain —
  unless the λ declares one free of aliases: `func.func @f(…) noalias [0]`, what
  C's `restrict` becomes. Nothing else the function names is that object, so it
  keeps a chain of its own. `ptr.disjoint %a, %na, %b, %nb : !i1` is the same
  fact where the proof is a runtime check rather than a qualifier: it is
  `a+na <= b || b+nb <= a` over unsigned addresses, which says `[a, a+na)` and
  `[b, b+nb)` share no byte as long as neither range wraps past the end of the
  address space — the producer's obligation, not the op's. It reads addresses,
  not memory, so it is pure and takes no state, and the backend prologue lowers
  it to the compares it stands for.
- An object that would be the whole of *exposed* memory as far as the order goes
  keeps no chain of its own: it would be named twice, once as itself and once as
  the conservative chain a call and a return still name, for no ordering gained.
- A call, a `memcpy` and a `return` touch every object the outside can reach.
  Those chains cross such an op through its single port: `state.join` merges them
  into the state it observes, `state.split` names each of them again in the state
  it leaves. A slot whose address never left the function is not among them.
- Disjoint chains never appear in each other's terms; their independence is
  structural (law S3), not something an alias analysis rediscovers downstream.

### 6.3 Ordering semantics

Memory order **is** the state DAG. A region's op order is one linearization of
it: SSA already forces def-before-use, so any order the edges admit is the same
program, and a pass may re-linearize under them.

Reads do not serialize. After a write — or an entry state, an allocation, a join,
a carried argument — any number of reads observe the state it left. That is a
*fork*: the reads are ordered against the write and against nothing else,
including each other. The next write, call, export or carried port on that chain
takes `state.join` of what the fork left, and that join edge is the WAR
dependence. RAW and WAW are the chain edge.

That the edges are complete is checked rather than argued. An unordered region
has no order to shuffle, so the check sits in the backend: the
`shuffle-machine-order` oracle re-linearizes every machine block by a seeded
random topological order of its dependence DAG, after selection and again after
allocation, and the differential fuzzer runs a variant with it on. A missing
edge is a divergence with a reproducer.

### 6.4 Discipline (verified)

- A dependency names the memory at one point, so at most one operation may
  *change* it: a second would describe two futures for one memory. Everything
  else naming it observes it — a read, or a `state.join`, which names the memory
  its inputs merge into — and any number of those may. One operation naming a
  state twice observes it once: joining a memory with itself is that memory.
- The check is structural, so it cannot see chains. That every access of a chain
  is in the cone of the state its next write takes is what `verify-deps` and
  the `shuffle-machine-order` oracle are for; the verifier is not that net.
- A state crossing a region boundary does so as a carried argument, which is a
  fresh value, so the check is one walk of the whole tree. A machine block
  parameter is the same fresh value, and a branch forwarding a state along an
  edge changes nothing, so the walk describes machine IR too — it is run there
  by `verify_machine_ir`, which the generic op-tree verifier does not reach.
- What an operation does to the memory it names is read off whichever fact it
  carries: the memory interfaces for a mid-end op, the
  [`InstrInfo`](instruction_selection.md) effects for a machine instruction.
  One that writes nothing observes — a load leaves memory as it found it, and a
  branch only carries it along the edge it takes.
- A read's published state *is* the state it observed, so a transform erasing a
  read hands its readers — joins included — the state it took.
- A store is *dead* where its own state is read by nothing at all — the commit's
  sweep — and where the slot it writes has no reader, in which case the
  allocation is dead too: with neither its address nor its state read, the object
  is one nothing in the function can tell exists, and DCE takes it. What leaves a
  write unread is the commit (§6.5): it moves a write back over the run of writes
  above it that name the extent it overwrites, where nothing else reads what they
  left.

### 6.5 The state laws

Definitional, like `Add` means addition — stated as the meaning of reads and
writes over the state algebra, not discharged by the SMT oracle (the
bit-blaster has no memory model). What keeps them honest: both sides read
the state operand, so a law that fires has already been told the accesses
alias exactly.

Both laws read the *extent* an access names — the object its address is
derived from, the byte offset into it, and the byte count — rather than the
address class alone, so `p + 4` and `p + 2 + 2` are the one extent they are.

- **S1** load-over-store forwarding: `Load(Store(s,a,n,v), a, n) = v` at one
  IR type.
- **S2** was dead-store elimination, stated as "the overwritten write leaves the
  state it was handed". It is gone. That is a claim about who may observe a
  memory, not an equality between two, and the negated conditions fencing it
  could each be unsaid by a merge a round later — which dropped whole chains of
  writes rather than the overwritten one. Dead-store elimination is now the
  commit's: it asks the saturated graph whether two writes name one extent
  (§6.4's placement, the same facts S1 reads), walks the chain in the IR where
  the answer is yes and nothing else reads what a write left, and lets DCE take
  what is then unread.
- **S3** disjoint-chain commutation: structural; asserted by test.

Every *other* memory equality remains an SMT obligation or does not exist.
Scalar promotion is not a law: it is construction (§5.1's demand annotation,
the `promote-nodes` pass), and by the time the view is built a local slot's
value is already a value the regions carry on ports.

## 7. Red views

### 7.1 The contract

A red view is a derived structure over the green core:

- Built on demand from a green subtree at a specific `(OpId, version)`;
  cached under that key; stale keys are misses, not events.
- Allocates **nothing** into the Context while being built or queried. (The
  historical "probe" hack — minting values to ask interface questions — is
  forbidden; views walk real ops.)
- Read-only views (dominance, `DefUse`) answer queries. Mutating views
  (the e-graph) change the program only through §2.5's atomic commit, or
  discard silently. Between build and commit a view may diverge from the
  green truth arbitrarily; mid-saturation an e-graph is not IR.
- A pass that never asks for a view never pays for it.

Views are ordinary structs with a build function. There is no view
framework, no view registry, no view base class.

### 7.2 The e-graph view

The single optimizer substrate: one seeder, one vocabulary, one driver, two
consumers (the mid-end canonicalizer and instruction selection).

**Vocabulary.** `SemNode` (`core/src/sem`): `Kind::Ir` (op-identity terms),
`Kind::Sym` (semantic terms: arithmetic, `If`, `Theta`, `LoadMemory`,
`StoreMemory`, …). Identity is `(kind, payload, type)`; provenance lives
outside identity.

**Seeding** walks a region's real ops through interfaces:

- Pure ops seed as op identity ∪ their `sem:` expansion (both terms, one
  class).
- `Conditional` seeds each result port as an `If`(decision, per-arm yields)
  term where the port is speculatable; otherwise the port anchors.
- `LoopLike` seeds each carried *state* port as a `Theta(init, latch)`
  projection. A value port every edge carries unchanged is unioned with what
  the loop was entered on; one the body changes is recorded for the
  hypothesis rounds below, which is what keeps the body argument and the
  loop's result distinct terms.
- Head-controlled loops compose: `If(guard₀, Theta(init, latch), init)` per
  port, with the guard term built in the e-graph from `GuardedLoop` —
  `lb < ub` for counted loops, the condition region seeded over the inits
  for general ones. No IR is rewritten to make this seeding possible.
- Memory ops seed as `LoadMemory(addr, bytes, meta, state)` /
  `StoreMemory(addr, bytes, value, space, state)` over the actual dependency
  edges, unioned with op identity. Identity *is* the state operand: loads
  agreeing on address and chain hash-cons; loads on different chains never
  meet.
- Region arguments and unmodeled ops anchor.

**Saturation** runs one driver over three rule sources, all of them PDL: the
peephole ruleset, the axiom theory (SMT-verified rewrites, plus each target's
own rule file), and the state laws (§6.5). Loop unrolling is
banned as a saturation rule (non-terminating); it is a structural clone via
the tree-edit API.

**Hypothesis scopes** are the optimism a sparse conditional solver has and a
bottom-up saturation does not. Before the base graph is read, each loop's
value ports are *hypothesised* to hold the constant the loop is entered on;
the body saturates under that assumption in a scope of its own; a port some
edge back into it refutes is dropped and the round runs again; what survives
is unioned into the base graph. A nest is resolved under its own enclosing
scope — an inner port entered on what an outer one carries is constant only
while the outer hypothesis is open, and an outer port latched from an inner
loop's result is unrefuted only once the inner one is proved. The mechanism
is a lattice fact stated as a term, so it extends to any fact a term can
state.

**Extraction and commit.** Cost-based extraction (`OpCost`, class-dependent
costs); commit stages a replacement region and lands it atomically (§2.5),
or discards if not improved. Two properties matter:

- A rewrite reroutes the readers of a value, and the operation that computed
  it — and every operation only that one read — is dead from that moment.
  The cascade is part of the commit, worklist-style, so the mid-end needs no
  DCE pass. A write is dead the same way: the state it publishes is the whole
  of what anything can observe about it.
- Commit rewires values, not only op results: a block argument, one result of
  many, a port a hypothesis proved constant. That is what makes constant
  propagation an instance of the same commit rather than a pass of its own.

**Consumers.** The canonicalizer pass ("instcombine") is a thin
build–saturate–extract–commit driver. Instruction selection builds the same
view per function, saturates with the target ruleset, and covers classes
with PBQP (§9). Value numbering is not a pass anywhere: hash-consing at
view construction *is* value numbering; commit is the elimination.

### 7.3 Other views

- **`DefUse`** — the only use/def index in the system, version-keyed. The
  green core does not maintain use lists; RAUW consults this view.
- **Dominance** — machine-CFG analyses for the backend (regalloc,
  liveness). The mid-end has no dominance consumer.
- **Dependence** (backend) — per-block dependence graph over machine ops
  (register def/use plus memory constraints) feeding the scheduler.
- **Affine** (`analysis::affine`) — iteration-space view over a maximal
  `CountedLoop` nest with intact bounds, which is the reason counted loops
  are not rotated away. Per depth the bounds as affine forms over the outer
  counters and the values the nest was entered with; per `MemoryRead`/
  `MemoryWrite` the chain its dependency operand is rooted at, the object its
  address is derived from and the offset into it; per pair of accesses on
  one chain the distances the single-equation GCD/bounded test admits, a
  range-disjointness predicate where the two objects differ, or nothing
  where it cannot decide. Refusal is per access and per pair. Built on
  demand and thrown away; `tir opt --print-affine` prints it.

## 8. What deliberately does not exist

For developers arriving from the previous architecture, the removed pieces
and their single survivors:

| Gone | Why | Survivor |
|---|---|---|
| `core/src/sea` (parallel IR: arenas, kinds, raise/lower, view, commit) | second IR ≠ the IR | this design |
| GSA / `gated_ssa.rs`, `SemNode::Merge(η/φ)` | derived gates from CFG; the structured form *states* gates | interfaces, read by the seeder |
| `mem2reg` (both variants) | dominance/φ machinery obsolete by structure: the region tree *is* the dominance, and a local slot's value belongs on the ports construction should have put it on | `promote-nodes` (demand annotation) over the chain `restructure-nodes` drew + shared escape classifier |
| mid-end DCE pass | a rewrite's cascade belongs to the commit that caused it | the commit's sweep (DCE remains for machine IR) |
| `sccp` + `ConstantFacts` | a second engine for a fact the first one can state: constants are classes, reachability is a gate's own scope | the e-graph's scopes, hypothesis rounds included |
| `dse` | same-chain overwrite is an extent question the placement facts answer; a slot with no reader is a dead definition | §6.5 + DCE on chains |
| `scf_to_cfg` + `cfg_cleanup` | destruction lives inside emission and emits clean CFG once | destruction-at-emission |
| three e-graph seeders (instcombine's, isel's `SemDagBuilder`, the sea view) | one program, one seeding | the §7.2 seeder |
| eager `Value::uses` in the green core | derived data in ground truth | `DefUse` view |
| `IRBuilder` + ad-hoc Context mutators + per-pass port-growing helpers | five mutation surfaces | the tree-edit API |
| PBQP in `core` with dead coherence machinery | generic math stranded behind the compiler | `tir-pbqp` utils crate (§9) |
| `DominatingEdgeFacts` | dominator-scoped facts on a CFG the mid-end no longer has | gate-context scoping in selection |
| loop-rotation / scf canonicalization | γ∘θ is the *view's* reading, not an IR shape | guard-aware seeding |
| fcc's AST-level goto machinery (per-construct predicate insertion, one refused shape) | one restructurer for one frontend, with a hole | the total `restructure-nodes` pass (§5.2) |

The pattern behind every row: compute a thing in one place, from the form
that states it most directly, and store it in exactly one representation.

## 9. The backend in one paragraph each

**Selection.** Per function: build the §7.2 view, saturate with the
TMDL-generated target ruleset, choose a cover with PBQP (`tir-pbqp` — a
generic node/edge/matrix cost solver with no IR dependency; regalloc and
selection build their own matrices on their side of the crate boundary),
emit machine ops per region, and destruct structured control to machine CFG
during emission (nothing an arm computes is speculated above its branch;
guarded edge-argument placement follows the established placement rule). State edges
are consumed for memory-op identity during selection and erased in emission.

**Scheduling.** Within a machine block, program order is a *derived*
property: the dependence view plus the TMDL `MachineModel` drive a list
scheduler (post-RA first) that decides the final order. This is the
"sea of nodes" property of the assembly stage — order comes from dependence
and the machine model, not from history.

**Register allocation.** PBQP-based, unchanged in architecture: liveness →
interference/affinity matrices → `tir-pbqp` solve → spill/retry.

## 10. Verification and determinism

- Op verifiers + interface verifiers run under the pass manager: on by
  default in debug builds, spine-scoped via versions, `TIR_VERIFY_IR`
  overrides in both directions. Failures name the pass that produced the
  broken IR.
- Rewrite soundness is tiered: axioms are SMT-proved (at rule-load / CI /
  `TIR_VERIFY_AXIOMS`); θ-facts are proved by induction in the axiom
  prover; state laws are definitional (§6.5); speculation is guarded at
  selection legality.
- Determinism is a hard requirement: identical input produces bit-identical
  output. Dense ids and ordered containers by construction; no iteration
  over unordered maps into anything that reaches output. The standing test
  is compile-twice-and-diff.

## 11. Recipes

**Add an op.** Declare it with `operation!` in its dialect: specs, verifier,
format, interfaces, and `sem:` if it has value semantics. With `sem:` it
folds and participates in e-graph reasoning with no further work. Register
nothing anywhere else.

**Give a dialect structured control flow.** Implement `Conditional` /
`LoopLike` + `GuardedLoop` (+ `TokenScope`) on your ops. The optimizer, promotion, selection, and destruction consume the
interfaces; your dialect appears nowhere in core.

**Write an optimization.** If it is an equality over values or memory:
a PDL rule (proved or definitional per §10), picked up by saturation —
never a hand-rolled traversal. If it is structural (unrolling, inlining,
outlining): a pass using the tree-edit API, cloning and splicing regions,
with the port-edit primitive for signature changes.

**Add an analysis.** A red view: a struct + build function keyed
`(OpId, version)`, registered with the analysis manager. Read real ops via
interfaces; allocate nothing into the Context; never cache across versions
yourself.

**Consume memory semantics.** Read `MemoryRead`/`MemoryWrite` state
accessors and follow dependency edges. Do not re-derive aliasing from
addresses; chain membership *is* the aliasing statement.
