/-!
# CDSL v3 Formal Specification: TEGG v15 Bytecode Validator Contract

Standalone, implementation-agnostic formal specification of the invariants
enforced by `tir_egglog::Program::from_bytes` (`libs/egglog/src/vm/file.rs`):
1. Bottom-up DAG child ordering (`file.rs:1013`): `∀ id c, c ∈ nodes[id].children → c < id`
2. Sequential register binding (`file.rs:1226-1233`): including repeated variables within one scan
3. 4-byte aligned binary layout (`file.rs:774`): `bytes % 4 = 0`

Any TEGG emitter implementation (today's OCaml prototype via `ocaml2lean`, or
tomorrow's production Rust emitter via Hax -> Lean 4) proves correctness by
constructing a `TeggConformance impl` witness against this unchanged spec.
-/

namespace CdslV3Spec

/-- Canonical specification node (polymorphic over payload `α`). -/
structure VmNode (α : Type) where
  payload  : α
  children : List Nat
  deriving DecidableEq, Repr

/-- Exact validator predicate from `libs/egglog/src/vm/file.rs:1013`:
    `children.iter().all(|&child| (child as usize) < id)`. -/
def ValidChildOrder {α : Type} (nodes : List (VmNode α)) : Prop :=
  ∀ (id : Nat) (node : VmNode α), nodes[id]? = some node → ∀ c ∈ node.children, c < id

/-- Canonical register binding directive (`file.rs:1226-1233`). -/
inductive Bind where
  | new (reg : Nat)
  | same (reg : Nat)
  deriving DecidableEq, Repr

/-- Exact mirror of `bind` check in `libs/egglog/src/vm/file.rs:1226-1233`:
    `Bind::New(r)` MUST satisfy `r == w` and advances `w` to `w + 1`;
    `Bind::Same(r)` MUST satisfy `r < w` and keeps `w` unchanged. -/
def checkBinds (w : Nat) : List Bind → Option Nat
  | [] => some w
  | .new r :: rest => if r == w then checkBinds (w + 1) rest else none
  | .same r :: rest => if r < w then checkBinds w rest else none

/-- Symbolic column request in a positive `Scan`. -/
inductive ColReq where
  | fresh
  | known (r : Nat)
  deriving DecidableEq, Repr

/-- Well-scopedness of a sequence of scan column requests starting at register water-mark `w`.
    Strictly stronger than requiring `r < initial_w`: also permits later columns in the same
    scan row to reference a register freshly bound by an earlier column (e.g. `xori(x, x)`). -/
def ValidReqs (w : Nat) : List ColReq → Prop
  | [] => True
  | .fresh :: rest => ValidReqs (w + 1) rest
  | .known r :: rest => r < w ∧ ValidReqs w rest

theorem validReqs_of_all_lt (w : Nat) (reqs : List ColReq)
    (h : ∀ r, .known r ∈ reqs → r < w) : ValidReqs w reqs := by
  induction reqs generalizing w with
  | nil => trivial
  | cons req rest ih =>
      cases req with
      | fresh =>
          apply ih (w + 1)
          intro r hr
          have := h r (List.mem_cons_of_mem _ hr)
          omega
      | known r =>
          refine ⟨h r List.mem_cons_self, ih w ?_⟩
          intro r' hr'
          exact h r' (List.mem_cons_of_mem _ hr')

/-- Abstract interface that ANY TEGG v15 emitter implementation (OCaml or Rust) provides. -/
structure TeggImpl where
  buildChain       : ∀ {α : Type}, List (VmNode α) → α → List α → List (VmNode α) × Nat
  allocBinds       : Nat → List ColReq → List Bind × Nat
  pad4             : Nat → Nat
  stringEntryBytes : Nat → Nat
  teggFileBytes    : List Nat → Nat → Nat

/-- **Formal TEGG v15 Conformance Contract:**
    Proving `TeggConformance impl` certifies that `impl` satisfies all three
    `tir_egglog::Program::from_bytes` validator invariants for ALL inputs. -/
structure TeggConformance (impl : TeggImpl) : Prop where
  /-- Validator invariant #1 (`file.rs:1013`): bottom-up DAG child ordering. -/
  buildChain_valid :
    ∀ {α : Type} (nodes : List (VmNode α)) (leaf : α) (parents : List α),
      ValidChildOrder nodes →
      ValidChildOrder (impl.buildChain nodes leaf parents).1 ∧
      (impl.buildChain nodes leaf parents).2 < (impl.buildChain nodes leaf parents).1.length
  /-- Validator invariant #2 (`file.rs:1226-1233`): sequential register allocation. -/
  allocBinds_valid :
    ∀ (w : Nat) (reqs : List ColReq),
      ValidReqs w reqs →
      checkBinds w (impl.allocBinds w reqs).1 = some (impl.allocBinds w reqs).2
  /-- Validator invariant #3a (`file.rs:774`): per-string 4-byte alignment. -/
  stringEntryBytes_aligned :
    ∀ (len : Nat), impl.stringEntryBytes len % 4 = 0
  /-- Validator invariant #3b (`file.rs:774`): full TEGG file 4-byte alignment. -/
  teggFileBytes_aligned :
    ∀ (strLens : List Nat) (wordCount : Nat), impl.teggFileBytes strLens wordCount % 4 = 0

/-- Reusable helper lemma for proving `ValidChildOrder` when appending one node. -/
theorem validChildOrder_append_one {α : Type} {nodes : List (VmNode α)} {nd : VmNode α}
    (hvalid : ValidChildOrder nodes)
    (hnd : ∀ c ∈ nd.children, c < nodes.length) :
    ValidChildOrder (nodes ++ [nd]) := by
  intro id node hget c hc
  by_cases hlt : id < nodes.length
  · rw [List.getElem?_append_left hlt] at hget
    exact hvalid id node hget c hc
  · have hge : nodes.length ≤ id := Nat.le_of_not_lt hlt
    rw [List.getElem?_append_right hge] at hget
    cases hdiff : id - nodes.length with
    | zero =>
        rw [hdiff] at hget
        simp at hget
        subst hget
        have hid : id = nodes.length := by omega
        rw [hid]
        exact hnd c hc
    | succ k =>
        rw [hdiff] at hget
        simp at hget

end CdslV3Spec
