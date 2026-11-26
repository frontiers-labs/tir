import CdslV3.Extraction.Funs
import CdslV3.Spec.Core
import CdslV3.Spec.KernelSpec
import CdslV3.Spec.TeggSpec

/-!
# Rust CDSL v3 Conformance Proofs (`tir_cdsl_core` ⊨ `CdslV3Spec`)

Proves that the Hax-extracted production Rust implementations in
`utils/cdsl/core/src/kernel_core.rs` and `utils/cdsl/core/src/tegg_core.rs`
satisfy the standalone `CdslV3Spec` contracts (`KernelSpec` and `TeggSpec`)
with zero `sorryAx` and zero custom axioms.
-/

open Aeneas.Std hiding namespace core alloc
open Aeneas.Std.WP
open RustM CoreModels
open tir_cdsl_core
open CdslV3Spec

set_option backward.isDefEq.respectTransparency false

namespace CdslV3.Verification

/-! ## Helpers for `RustM`, `Usize` arithmetic, and `CoreModels` sequences -/

private theorem wp_to_ok {α : Type u} {m : RustM α} {P : α → Prop} (h : WP.spec m P) :
    ∃ x, m = ok x ∧ P x := by
  exact WP.spec_imp_exists h

private theorem ok_to_wp {α : Type u} {x : α} {P : α → Prop} (h : P x) :
    WP.spec (ok x : RustM α) P := by
  simpa using h

private theorem ok_inj {α : Type u} {a b : α} (h : (ok a : RustM α) = ok b) : a = b :=
  RustM.ok_injective h

private theorem usize_add_ok (a b : Usize) (h : a.val + b.val ≤ Usize.max) :
    ∃ c : Usize, a + b = ok c ∧ c.val = a.val + b.val := by
  obtain ⟨c, hc, hval, _⟩ := wp_to_ok (UScalar.add_bv_spec (x := a) (y := b) (by scalar_tac))
  exact ⟨c, hc, hval⟩

private theorem usize_sub_ok (a b : Usize) (h : b.val ≤ a.val) :
    ∃ c : Usize, a - b = ok c ∧ c.val = a.val - b.val := by
  obtain ⟨c, hc, hval, _⟩ := wp_to_ok (UScalar.sub_bv_spec (x := a) (y := b) h)
  exact ⟨c, hc, hval⟩

private theorem usize_mul_ok (a b : Usize) (h : a.val * b.val ≤ Usize.max) :
    ∃ c : Usize, a * b = ok c ∧ c.val = a.val * b.val := by
  obtain ⟨c, hc, hval, _⟩ := wp_to_ok (UScalar.mul_bv_spec (x := a) (y := b) (by scalar_tac))
  exact ⟨c, hc, hval⟩

private theorem usize_rem_ok (a b : Usize) (h : b.val ≠ 0) :
    ∃ c : Usize, a % b = ok c ∧ c.val = a.val % b.val :=
  wp_to_ok (UScalar.rem_spec a h)

private theorem slice_from_val {T : Type} (l : List T) (h : l.length ≤ Usize.max) :
    (Slice.from l h).val = l := by
  simp [Slice.from, Slice.val, Aeneas.Data.ListN.ListN.from_to_inverse]

private theorem vec_new_ok (T : Type) :
    ∃ v : alloc.vec.Vec T, alloc.vec.Vec.new T = ok v ∧ v.val = [] := by
  refine ⟨Slice.new T, ?_, rfl⟩
  simp [alloc.vec.Vec.new, rust_primitives.sequence.seq_empty]

private theorem vec_push_ok {T : Type} (v : alloc.vec.Vec T) (x : T)
    (h : v.val.length + 1 ≤ Usize.max) :
    ∃ v' : alloc.vec.Vec T, alloc.vec.Vec.push v x = ok v' ∧ v'.val = v.val ++ [x] := by
  have hlt : v.val.length < Usize.max := by omega
  have hb : (v.val ++ [x]).length ≤ Usize.max := by simpa [List.length_append] using h
  refine ⟨Slice.from (v.val ++ [x]) hb, ?_, slice_from_val _ hb⟩
  simp [alloc.vec.Vec.push, rust_primitives.sequence.seq_push, hlt]

private theorem vec_len_ok {T : Type} (v : alloc.vec.Vec T) :
    ∃ n : Usize, alloc.vec.Vec.len v = ok n ∧ n.val = v.val.length := by
  refine ⟨Usize.ofNatCore v.val.length (by have := v.property; scalar_tac), ?_, rfl⟩
  simp [alloc.vec.Vec.len, rust_primitives.sequence.seq_len, Slice.len]

private theorem vec_pop_ok {T : Type} (v : alloc.vec.Vec T) :
    (v.val.length = 0 → alloc.vec.Vec.pop v = ok (.None, v)) ∧
    (0 < v.val.length →
      ∃ (x : T) (v' : alloc.vec.Vec T),
        alloc.vec.Vec.pop v = ok (.Some x, v') ∧ v'.val.length = v.val.length - 1) := by
  constructor
  · intro h0
    unfold alloc.vec.Vec.pop
    have hslen : rust_primitives.sequence.seq_len v =
        ok (Usize.ofNatCore v.val.length (by have := v.property; scalar_tac)) := by
      simp [rust_primitives.sequence.seq_len, Slice.len]
    rw [hslen]
    simp only [Aeneas.Std.bind_ok]
    have hng : ¬ (Usize.ofNatCore v.val.length (by have := v.property; scalar_tac) > 0#usize) := by
      change ¬ (0 < v.val.length); omega
    simp [hng]
  · intro hpos
    unfold alloc.vec.Vec.pop
    have hslen : rust_primitives.sequence.seq_len v =
        ok (Usize.ofNatCore v.val.length (by have := v.property; scalar_tac)) := by
      simp [rust_primitives.sequence.seq_len, Slice.len]
    have hgt : Usize.ofNatCore v.val.length (by have := v.property; scalar_tac) > 0#usize := by
      change 0 < v.val.length; exact hpos
    have h1 : (1#usize : Usize).val = 1 := rfl
    obtain ⟨idx, hsub, hidx_val⟩ := usize_sub_ok
      (Usize.ofNatCore v.val.length (by have := v.property; scalar_tac)) 1#usize (by change 1 ≤ v.val.length; omega)
    have hidx_lt : idx.val < v.val.length := by
      have hv_len : (Usize.ofNatCore v.val.length (by have := v.property; scalar_tac)).val = v.val.length := rfl
      omega
    let v1 : alloc.vec.Vec T := Slice.from (v.val.take idx.val ++ v.val.drop (idx.val + 1)) (by
      simp only [List.length_append, List.length_take, List.length_drop]
      have := v.property; omega)
    have hrem : rust_primitives.sequence.seq_remove v idx = ok (v.val.get ⟨idx.val, hidx_lt⟩, v1) := by
      simp [rust_primitives.sequence.seq_remove, hidx_lt, v1]
    refine ⟨v.val.get ⟨idx.val, hidx_lt⟩, v1, ?_, ?_⟩
    · simp only [hslen, hgt, ↓reduceIte, hsub, hrem, Aeneas.Std.bind_ok]
      rfl
    · rw [slice_from_val]
      simp only [List.length_append, List.length_take, List.length_drop]
      omega

private theorem slice_len_ok {T : Type} (s : Slice T) :
    ∃ n : Usize, core.slice.Slice.len s = ok n ∧ n.val = s.val.length := by
  refine ⟨Usize.ofNatCore s.val.length (by have := s.property; scalar_tac), ?_, rfl⟩
  simp [core.slice.Slice.len, rust_primitives.slice.slice_length, Slice.len]

private theorem slice_reverse_ok {T : Type} (s : Slice T) :
    ∃ s' : Slice T, core.slice.Slice.reverse s = ok s' ∧ s'.val = s.val.reverse := by
  have hb : s.val.reverse.length ≤ Usize.max := by simp [s.property]
  refine ⟨Slice.from s.val.reverse hb, ?_, slice_from_val _ hb⟩
  simp [core.slice.Slice.reverse, rust_primitives.slice.slice_reverse]

/-! ## Part 1: Kernel Core (`tir_cdsl_core::kernel_core` ⊨ `CdslV3Spec.KernelSpec`) -/

def toRustTy : CdslV3Spec.Ty → kernel_core.Ty
  | .int => .Int
  | .bool => .Bool
  | .proof => .Proof
  | .opt e => .Opt (toRustTy e)
  | .arr d c => .Arr (toRustTy d) (toRustTy c)

def fromRustTy : kernel_core.Ty → CdslV3Spec.Ty
  | .Int => .int
  | .Bool => .bool
  | .Proof => .proof
  | .Opt e => .opt (fromRustTy e)
  | .Arr d c => .arr (fromRustTy d) (fromRustTy c)

def toRustCtx : CdslV3Spec.Ctx → kernel_core.TyList
  | [] => .Nil
  | ty :: rest => .Cons (toRustTy ty) (toRustCtx rest)

def fromRustCtx : kernel_core.TyList → CdslV3Spec.Ctx
  | .Nil => []
  | .Cons ty rest => fromRustTy ty :: fromRustCtx rest

def toRustOptTy : Option CdslV3Spec.Ty → core.option.Option kernel_core.Ty
  | none => .None
  | some t => .Some (toRustTy t)

def fromRustOptTy : core.option.Option kernel_core.Ty → Option CdslV3Spec.Ty
  | .None => none
  | .Some t => some (fromRustTy t)

def fromRustTerm : kernel_core.Term → CdslV3Spec.Term
  | .Var i => .var i.val
  | .LitInt n => .litInt n.val
  | .LitBool b => .litBool b
  | .Add a b => .add (fromRustTerm a) (fromRustTerm b)
  | .Lam d b => .lam (fromRustTy d) (fromRustTerm b)
  | .App f a => .app (fromRustTerm f) (fromRustTerm a)
  | .LetE ty r b => .letE (fromRustTy ty) (fromRustTerm r) (fromRustTerm b)
  | .NoneE t => .none (fromRustTy t)
  | .SomeE v => .some (fromRustTerm v)
  | .MatchOpt s n c => .matchOpt (fromRustTerm s) (fromRustTerm n) (fromRustTerm c)
  | .MkProof => .mkProof
  | .MatchProof s b => .matchProof (fromRustTerm s) (fromRustTerm b)

def fromRustML : kernel_core.MlExpr → CdslV3Spec.MLExpr
  | .Var i => .var i.val
  | .LitInt n => .litInt n.val
  | .LitBool b => .litBool b
  | .Add a b => .add (fromRustML a) (fromRustML b)
  | .Lam b => .lam (fromRustML b)
  | .App f a => .app (fromRustML f) (fromRustML a)
  | .LetE r b => .letE (fromRustML r) (fromRustML b)
  | .None => .none
  | .Some v => .some (fromRustML v)
  | .MatchOpt s n c => .matchOpt (fromRustML s) (fromRustML n) (fromRustML c)
  | .Erased => .erased

private theorem from_to_ty (t : CdslV3Spec.Ty) : fromRustTy (toRustTy t) = t := by
  induction t with
  | int => rfl
  | bool => rfl
  | proof => rfl
  | opt e ih => simp [toRustTy, fromRustTy, ih]
  | arr d c ihd ihc => simp [toRustTy, fromRustTy, ihd, ihc]

private theorem to_from_ty (t : kernel_core.Ty) : toRustTy (fromRustTy t) = t := by
  induction t with
  | Int => rfl
  | Bool => rfl
  | Proof => rfl
  | Opt e ih => simp [toRustTy, fromRustTy, ih]
  | Arr d c ihd ihc => simp [toRustTy, fromRustTy, ihd, ihc]

private theorem rust_ty_clone (t : kernel_core.Ty) : kernel_core.ty_clone t = ok t := by
  induction t with
  | Int => rw [kernel_core.ty_clone]
  | Bool => rw [kernel_core.ty_clone]
  | Proof => rw [kernel_core.ty_clone]
  | Opt e ih =>
    rw [kernel_core.ty_clone]
    simp [ih, Aeneas.Std.bind_ok]
  | Arr d c ihd ihc =>
    rw [kernel_core.ty_clone]
    simp [ihd, ihc, Aeneas.Std.bind_ok]

private theorem rust_ty_list_clone (xs : kernel_core.TyList) :
    kernel_core.ty_list_clone xs = ok xs := by
  induction xs with
  | Nil => rw [kernel_core.ty_list_clone]
  | Cons h t ih =>
    rw [kernel_core.ty_list_clone]
    simp [rust_ty_clone, ih, Aeneas.Std.bind_ok]

private theorem rust_ty_eq (a b : kernel_core.Ty) :
    kernel_core.ty_eq a b = ok (decide (fromRustTy a = fromRustTy b)) := by
  induction a generalizing b with
  | Int =>
    cases b <;> (rw [kernel_core.ty_eq]; simp [fromRustTy])
  | Bool =>
    cases b <;> (rw [kernel_core.ty_eq]; simp [fromRustTy])
  | Proof =>
    cases b <;> (rw [kernel_core.ty_eq]; simp [fromRustTy])
  | Opt ae ih =>
    cases b with
    | Opt be =>
      rw [kernel_core.ty_eq]
      simpa [fromRustTy] using ih be
    | _ =>
      rw [kernel_core.ty_eq]
      simp [fromRustTy]
  | Arr ad ac ihd ihc =>
    cases b with
    | Arr bd bc =>
      rw [kernel_core.ty_eq]
      simp only [ihd bd, Aeneas.Std.bind_ok]
      by_cases h : fromRustTy ad = fromRustTy bd
      · simp [h, ihc bc, fromRustTy]
      · simp [h, fromRustTy]
    | _ =>
      rw [kernel_core.ty_eq]
      simp [fromRustTy]

private theorem rust_ty_is_prop (t : kernel_core.Ty) :
    kernel_core.ty_is_prop t = ok (fromRustTy t).isProp := by
  induction t with
  | Int => rw [kernel_core.ty_is_prop]; rfl
  | Bool => rw [kernel_core.ty_is_prop]; rfl
  | Proof => rw [kernel_core.ty_is_prop]; rfl
  | Opt _ => rw [kernel_core.ty_is_prop]; rfl
  | Arr _ cod _ ihc =>
    rw [kernel_core.ty_is_prop]
    simpa [fromRustTy, CdslV3Spec.Ty.isProp] using ihc

theorem rust_prop_elim_allowed (s r : Bool) :
    kernel_core.prop_elim_allowed s r = ok (if s then r else true) := by
  unfold kernel_core.prop_elim_allowed
  cases s <;> rfl

private theorem rust_ctx_get (xs : kernel_core.TyList) (idx : Usize) :
    kernel_core.ctx_get xs idx = ok (toRustOptTy ((fromRustCtx xs)[idx.val]?)) := by
  induction xs generalizing idx with
  | Nil =>
    rw [kernel_core.ctx_get]
    simp [fromRustCtx, toRustOptTy]
  | Cons h t ih =>
    rw [kernel_core.ctx_get]
    by_cases hz : idx = 0#usize
    · rw [hz]
      have h0 : (0#usize : Usize).val = 0 := rfl
      simp [h0, rust_ty_clone, Aeneas.Std.bind_ok, fromRustCtx, toRustOptTy, to_from_ty]
    · have hpos : (1#usize : Usize).val ≤ idx.val := by
        change 1 ≤ idx.val
        by_contra hlt
        have hv : idx.val = 0 := by omega
        exact hz (UScalar.eq_of_val_eq hv)
      obtain ⟨idx', hsub, hval⟩ := usize_sub_ok idx 1#usize hpos
      simp only [hz, ↓reduceIte, hsub, Aeneas.Std.bind_ok]
      rw [ih idx']
      have hget : (fromRustTy h :: fromRustCtx t)[idx.val]? = (fromRustCtx t)[idx'.val]? := by
        cases hv : idx.val with
        | zero =>
          change 1 ≤ idx.val at hpos
          omega
        | succ k =>
          have h1 : (1#usize : Usize).val = 1 := rfl
          have hk : idx'.val = k := by omega
          simp [hk]
      simp [fromRustCtx, hget]

/-- Core refinement theorem: `tir_cdsl_core::kernel_core::infer_type` never panics
    and returns the exact same `Option Ty` as `CdslV3Spec.specInferType`. -/
theorem rust_infer_type_refines (xs : kernel_core.TyList) (t : kernel_core.Term) :
    kernel_core.infer_type xs t = ok (toRustOptTy (specInferType (fromRustCtx xs) (fromRustTerm t))) := by
  induction t generalizing xs with
  | Var i =>
    rw [kernel_core.infer_type]
    simpa [fromRustTerm, specInferType] using rust_ctx_get xs i
  | LitInt _ =>
    rw [kernel_core.infer_type]; rfl
  | LitBool _ =>
    rw [kernel_core.infer_type]; rfl
  | Add a b iha ihb =>
    rw [kernel_core.infer_type]
    simp only [iha xs, Aeneas.Std.bind_ok, fromRustTerm, specInferType]
    cases specInferType (fromRustCtx xs) (fromRustTerm a) with
    | none => simp [toRustOptTy]
    | some ta =>
      cases ta with
      | int =>
        simp only [toRustOptTy, toRustTy, ihb xs, Aeneas.Std.bind_ok]
        cases specInferType (fromRustCtx xs) (fromRustTerm b) with
        | none => simp
        | some tb =>
          cases tb <;> simp [toRustTy]
      | _ => simp [toRustOptTy, toRustTy]
  | Lam dom body ih =>
    rw [kernel_core.infer_type]
    simp only [rust_ty_clone, rust_ty_list_clone, Aeneas.Std.bind_ok, fromRustTerm, specInferType]
    have hb := ih (.Cons dom xs)
    simp only [fromRustCtx] at hb
    rw [hb]
    cases specInferType (fromRustTy dom :: fromRustCtx xs) (fromRustTerm body) with
    | none => simp [toRustOptTy]
    | some cod => simp [toRustOptTy, toRustTy, to_from_ty]
  | App f a ihf iha =>
    rw [kernel_core.infer_type]
    simp only [ihf xs, Aeneas.Std.bind_ok, fromRustTerm, specInferType]
    cases specInferType (fromRustCtx xs) (fromRustTerm f) with
    | none => simp [toRustOptTy]
    | some tf =>
      cases tf with
      | arr dom cod =>
        simp only [toRustOptTy, toRustTy, iha xs, Aeneas.Std.bind_ok]
        cases specInferType (fromRustCtx xs) (fromRustTerm a) with
        | none => simp
        | some ta =>
          simp only [rust_ty_eq, from_to_ty, Aeneas.Std.bind_ok]
          by_cases h : ta = dom
          · subst h
            simp [rust_ty_clone, Aeneas.Std.bind_ok]
          · simp [h]
      | _ => simp [toRustOptTy, toRustTy]
  | LetE ann rhs body ihr ihb =>
    rw [kernel_core.infer_type]
    simp only [ihr xs, Aeneas.Std.bind_ok, fromRustTerm, specInferType]
    cases specInferType (fromRustCtx xs) (fromRustTerm rhs) with
    | none => simp [toRustOptTy]
    | some tr =>
      simp only [toRustOptTy, rust_ty_eq, from_to_ty, Aeneas.Std.bind_ok]
      by_cases h : tr = fromRustTy ann
      · simp only [h, decide_true, ↓reduceIte, rust_ty_clone, rust_ty_list_clone, Aeneas.Std.bind_ok]
        have hb := ihb (.Cons ann xs)
        simpa [fromRustCtx, toRustOptTy] using hb
      · simp [h]
  | NoneE elem_ty =>
    rw [kernel_core.infer_type]
    simp [rust_ty_clone, Aeneas.Std.bind_ok, fromRustTerm, specInferType, toRustOptTy, toRustTy, to_from_ty]
  | SomeE v ih =>
    rw [kernel_core.infer_type]
    simp only [ih xs, Aeneas.Std.bind_ok, fromRustTerm, specInferType]
    cases specInferType (fromRustCtx xs) (fromRustTerm v) <;> simp [toRustOptTy, toRustTy]
  | MatchOpt s n c ihs ihn ihc =>
    rw [kernel_core.infer_type]
    simp only [ihs xs, Aeneas.Std.bind_ok, fromRustTerm, specInferType]
    cases specInferType (fromRustCtx xs) (fromRustTerm s) with
    | none => simp [toRustOptTy]
    | some ts =>
      cases ts with
      | opt elem =>
        simp only [toRustOptTy, toRustTy, rust_ty_clone, rust_ty_list_clone, ihn xs, Aeneas.Std.bind_ok]
        cases specInferType (fromRustCtx xs) (fromRustTerm n) with
        | none => simp
        | some tn =>
          have hc := ihc (.Cons (toRustTy elem) xs)
          simp only [fromRustCtx, from_to_ty] at hc
          simp only [toRustOptTy, hc, Aeneas.Std.bind_ok]
          cases specInferType (elem :: fromRustCtx xs) (fromRustTerm c) with
          | none => simp
          | some tc =>
            simp only [rust_ty_eq, from_to_ty, Aeneas.Std.bind_ok]
            by_cases h : tn = tc
            · subst h
              simp
            · simp [h]
      | _ => simp [toRustOptTy, toRustTy]
  | MkProof =>
    rw [kernel_core.infer_type]; rfl
  | MatchProof s b ihs ihb =>
    rw [kernel_core.infer_type]
    simp only [ihs xs, Aeneas.Std.bind_ok, fromRustTerm, specInferType]
    cases specInferType (fromRustCtx xs) (fromRustTerm s) with
    | none => simp [toRustOptTy]
    | some ts =>
      cases ts with
      | proof =>
        simp only [toRustOptTy, toRustTy, ihb xs, Aeneas.Std.bind_ok]
        cases specInferType (fromRustCtx xs) (fromRustTerm b) with
        | none => simp
        | some res =>
          simp only [rust_ty_is_prop, from_to_ty, rust_prop_elim_allowed, Aeneas.Std.bind_ok]
          cases res.isProp with
          | false => simp
          | true => simp [rust_ty_clone, Aeneas.Std.bind_ok]
      | _ => simp [toRustOptTy, toRustTy]

/-- Core refinement theorem: `tir_cdsl_core::kernel_core::erase_term` never panics
    and returns an `MlExpr` whose image equals `CdslV3Spec.specEraseTerm`. -/
theorem rust_erase_term_refines (xs : kernel_core.TyList) (t : kernel_core.Term) :
    ∃ m, kernel_core.erase_term xs t = ok m ∧
      fromRustML m = specEraseTerm (fromRustCtx xs) (fromRustTerm t) := by
  induction t generalizing xs with
  | Var i =>
    rw [kernel_core.erase_term, rust_infer_type_refines]
    unfold specEraseTerm
    simp only [fromRustTerm, Aeneas.Std.bind_ok]
    cases specInferType (fromRustCtx xs) (.var i.val) with
    | none => exact ⟨.Erased, by simp [toRustOptTy], rfl⟩
    | some ty =>
      simp only [toRustOptTy, rust_ty_is_prop, from_to_ty, Aeneas.Std.bind_ok]
      cases ty.isProp with
      | false => exact ⟨.Var i, by simp, rfl⟩
      | true => exact ⟨.Erased, by simp, rfl⟩
  | LitInt n =>
    rw [kernel_core.erase_term, rust_infer_type_refines]
    exact ⟨.LitInt n, by simp [fromRustTerm, specInferType, toRustOptTy, rust_ty_is_prop, from_to_ty, Aeneas.Std.bind_ok, CdslV3Spec.Ty.isProp], rfl⟩
  | LitBool b =>
    rw [kernel_core.erase_term, rust_infer_type_refines]
    exact ⟨.LitBool b, by simp [fromRustTerm, specInferType, toRustOptTy, rust_ty_is_prop, from_to_ty, Aeneas.Std.bind_ok, CdslV3Spec.Ty.isProp], rfl⟩
  | Add a b iha ihb =>
    rw [kernel_core.erase_term, rust_infer_type_refines]
    unfold specEraseTerm
    simp only [fromRustTerm, Aeneas.Std.bind_ok]
    cases specInferType (fromRustCtx xs) (.add (fromRustTerm a) (fromRustTerm b)) with
    | none => exact ⟨.Erased, by simp [toRustOptTy], rfl⟩
    | some ty =>
      simp only [toRustOptTy, rust_ty_is_prop, from_to_ty, Aeneas.Std.bind_ok]
      cases ty.isProp with
      | true => exact ⟨.Erased, by simp, rfl⟩
      | false =>
        obtain ⟨ma, ha1, ha2⟩ := iha xs
        obtain ⟨mb, hb1, hb2⟩ := ihb xs
        refine ⟨.Add ma mb, ?_, ?_⟩
        · simp [ha1, hb1, Aeneas.Std.bind_ok]
        · simp [fromRustML, ha2, hb2]
  | Lam dom body ih =>
    rw [kernel_core.erase_term, rust_infer_type_refines]
    unfold specEraseTerm
    simp only [fromRustTerm, Aeneas.Std.bind_ok]
    cases specInferType (fromRustCtx xs) (.lam (fromRustTy dom) (fromRustTerm body)) with
    | none => exact ⟨.Erased, by simp [toRustOptTy], rfl⟩
    | some ty =>
      simp only [toRustOptTy, rust_ty_is_prop, from_to_ty, Aeneas.Std.bind_ok]
      cases ty.isProp with
      | true => exact ⟨.Erased, by simp, rfl⟩
      | false =>
        obtain ⟨mb, hb1, hb2⟩ := ih (.Cons dom xs)
        refine ⟨.Lam mb, ?_, ?_⟩
        · simp [rust_ty_clone, rust_ty_list_clone, hb1, Aeneas.Std.bind_ok]
        · simpa [fromRustML, fromRustCtx] using hb2
  | App f a ihf iha =>
    rw [kernel_core.erase_term, rust_infer_type_refines]
    unfold specEraseTerm
    simp only [fromRustTerm, Aeneas.Std.bind_ok]
    cases specInferType (fromRustCtx xs) (.app (fromRustTerm f) (fromRustTerm a)) with
    | none => exact ⟨.Erased, by simp [toRustOptTy], rfl⟩
    | some ty =>
      simp only [toRustOptTy, rust_ty_is_prop, from_to_ty, Aeneas.Std.bind_ok]
      cases ty.isProp with
      | true => exact ⟨.Erased, by simp, rfl⟩
      | false =>
        obtain ⟨mf, hf1, hf2⟩ := ihf xs
        obtain ⟨ma, ha1, ha2⟩ := iha xs
        refine ⟨.App mf ma, ?_, ?_⟩
        · simp [hf1, ha1, Aeneas.Std.bind_ok]
        · simp [fromRustML, hf2, ha2]
  | LetE ann rhs body ihr ihb =>
    rw [kernel_core.erase_term, rust_infer_type_refines]
    unfold specEraseTerm
    simp only [fromRustTerm, Aeneas.Std.bind_ok]
    cases specInferType (fromRustCtx xs) (.letE (fromRustTy ann) (fromRustTerm rhs) (fromRustTerm body)) with
    | none => exact ⟨.Erased, by simp [toRustOptTy], rfl⟩
    | some ty =>
      simp only [toRustOptTy, rust_ty_is_prop, from_to_ty, Aeneas.Std.bind_ok]
      cases ty.isProp with
      | true => exact ⟨.Erased, by simp, rfl⟩
      | false =>
        obtain ⟨mr, hr1, hr2⟩ := ihr xs
        obtain ⟨mb, hb1, hb2⟩ := ihb (.Cons ann xs)
        refine ⟨.LetE mr mb, ?_, ?_⟩
        · simp [rust_ty_clone, rust_ty_list_clone, hr1, hb1, Aeneas.Std.bind_ok]
        · simp only [fromRustML, fromRustCtx] at hb2 ⊢
          simp [hr2, hb2]
  | NoneE elem_ty =>
    rw [kernel_core.erase_term, rust_infer_type_refines]
    exact ⟨.None, by simp [fromRustTerm, specInferType, toRustOptTy, rust_ty_is_prop, from_to_ty, Aeneas.Std.bind_ok, CdslV3Spec.Ty.isProp], rfl⟩
  | SomeE v ih =>
    rw [kernel_core.erase_term, rust_infer_type_refines]
    unfold specEraseTerm
    simp only [fromRustTerm, Aeneas.Std.bind_ok]
    cases specInferType (fromRustCtx xs) (.some (fromRustTerm v)) with
    | none => exact ⟨.Erased, by simp [toRustOptTy], rfl⟩
    | some ty =>
      simp only [toRustOptTy, rust_ty_is_prop, from_to_ty, Aeneas.Std.bind_ok]
      cases ty.isProp with
      | true => exact ⟨.Erased, by simp, rfl⟩
      | false =>
        obtain ⟨mv, hv1, hv2⟩ := ih xs
        refine ⟨.Some mv, ?_, ?_⟩
        · simp [hv1, Aeneas.Std.bind_ok]
        · simp [fromRustML, hv2]
  | MatchOpt s n c ihs ihn ihc =>
    rw [kernel_core.erase_term, rust_infer_type_refines]
    unfold specEraseTerm
    simp only [fromRustTerm, Aeneas.Std.bind_ok]
    cases specInferType (fromRustCtx xs) (.matchOpt (fromRustTerm s) (fromRustTerm n) (fromRustTerm c)) with
    | none => exact ⟨.Erased, by simp [toRustOptTy], rfl⟩
    | some ty =>
      simp only [toRustOptTy, rust_ty_is_prop, from_to_ty, Aeneas.Std.bind_ok]
      cases ty.isProp with
      | true => exact ⟨.Erased, by simp, rfl⟩
      | false =>
        simp only [Bool.false_eq_true, ↓reduceIte, rust_infer_type_refines, Aeneas.Std.bind_ok]
        cases specInferType (fromRustCtx xs) (fromRustTerm s) with
        | none => exact ⟨.Erased, by simp [toRustOptTy], rfl⟩
        | some ts =>
          cases ts with
          | opt elem =>
            obtain ⟨ms, hs1, hs2⟩ := ihs xs
            obtain ⟨mn, hn1, hn2⟩ := ihn xs
            obtain ⟨mc, hc1, hc2⟩ := ihc (.Cons (toRustTy elem) xs)
            refine ⟨.MatchOpt ms mn mc, ?_, ?_⟩
            · simp [toRustOptTy, toRustTy, rust_ty_clone, rust_ty_list_clone, hs1, hn1, hc1, Aeneas.Std.bind_ok]
            · simp only [fromRustCtx, from_to_ty] at hc2
              simp [fromRustML, hs2, hn2, hc2]
          | _ => exact ⟨.Erased, by simp [toRustOptTy, toRustTy], rfl⟩
  | MkProof =>
    rw [kernel_core.erase_term, rust_infer_type_refines]
    exact ⟨.Erased, by simp [fromRustTerm, specInferType, toRustOptTy, rust_ty_is_prop, Aeneas.Std.bind_ok], rfl⟩
  | MatchProof s b _ _ =>
    rw [kernel_core.erase_term, rust_infer_type_refines]
    unfold specEraseTerm
    simp only [fromRustTerm, Aeneas.Std.bind_ok]
    cases specInferType (fromRustCtx xs) (.matchProof (fromRustTerm s) (fromRustTerm b)) with
    | none => exact ⟨.Erased, by simp [toRustOptTy], rfl⟩
    | some ty =>
      simp only [toRustOptTy, rust_ty_is_prop, from_to_ty, Aeneas.Std.bind_ok]
      cases ty.isProp <;> exact ⟨.Erased, by simp, rfl⟩

/-- **Theorem 1a (Rust Prop-Elimination Guard Soundness)** -/
theorem rust_prop_elim_guard_sound (b : Bool)
    (h : kernel_core.prop_elim_allowed true b = ok true) : b = true := by
  rw [rust_prop_elim_allowed] at h
  simpa using ok_inj h

/-- **Theorem 1b (Rust Kernel Typechecker Soundness & Completeness)** -/
theorem rust_infer_type_sound (xs : kernel_core.TyList) (t : kernel_core.Term) (ty : kernel_core.Ty)
    (h : kernel_core.infer_type xs t = ok (.Some ty)) :
    HasType (fromRustCtx xs) (fromRustTerm t) (fromRustTy ty) := by
  rw [rust_infer_type_refines] at h
  have hinj := ok_inj h
  cases hs : specInferType (fromRustCtx xs) (fromRustTerm t) with
  | none => simp [hs, toRustOptTy] at hinj
  | some T =>
    simp [hs, toRustOptTy] at hinj
    subst hinj
    rw [from_to_ty]
    exact specInferType_sound _ _ _ hs

theorem rust_infer_type_complete (xs : kernel_core.TyList) (t : kernel_core.Term) (T : CdslV3Spec.Ty)
    (ht : HasType (fromRustCtx xs) (fromRustTerm t) T) :
    kernel_core.infer_type xs t = ok (.Some (toRustTy T)) := by
  rw [rust_infer_type_refines, specInferType_complete ht]
  rfl

/-- **Theorem 1c (Rust Prop Erasure, Simulation, and Value Preservation)** -/
theorem rust_erase_prop (xs : kernel_core.TyList) (t : kernel_core.Term) (T : CdslV3Spec.Ty)
    (ht : HasType (fromRustCtx xs) (fromRustTerm t) T) (hp : T.isProp = true) :
    ∃ m, kernel_core.erase_term xs t = ok m ∧ fromRustML m = .erased := by
  obtain ⟨m, hm, heq⟩ := rust_erase_term_refines xs t
  exact ⟨m, hm, heq.trans (specEraseTerm_prop ht hp)⟩

theorem rust_erase_simulation (t t' : kernel_core.Term) (T : CdslV3Spec.Ty)
    (ht : HasType [] (fromRustTerm t) T) (hsteps : StepStar (fromRustTerm t) (fromRustTerm t')) :
    ∃ m m', kernel_core.erase_term .Nil t = ok m ∧
      kernel_core.erase_term .Nil t' = ok m' ∧
      MLStepStar (fromRustML m) (fromRustML m') := by
  obtain ⟨m, hm, hmeq⟩ := rust_erase_term_refines .Nil t
  obtain ⟨m', hm', hm'eq⟩ := rust_erase_term_refines .Nil t'
  refine ⟨m, m', hm, hm', ?_⟩
  rw [hmeq, hm'eq]
  exact specEraseTerm_simulation ht hsteps

theorem rust_erase_preserves_int (t : kernel_core.Term) (n : Int)
    (ht : HasType [] (fromRustTerm t) .int) (hsteps : StepStar (fromRustTerm t) (.litInt n)) :
    ∃ m, kernel_core.erase_term .Nil t = ok m ∧ MLStepStar (fromRustML m) (.litInt n) := by
  obtain ⟨m, hm, hmeq⟩ := rust_erase_term_refines .Nil t
  refine ⟨m, hm, ?_⟩
  rw [hmeq]
  exact specEraseTerm_simulation ht hsteps

theorem rust_erase_preserves_bool (t : kernel_core.Term) (b : Bool)
    (ht : HasType [] (fromRustTerm t) .bool) (hsteps : StepStar (fromRustTerm t) (.litBool b)) :
    ∃ m, kernel_core.erase_term .Nil t = ok m ∧ MLStepStar (fromRustML m) (.litBool b) := by
  obtain ⟨m, hm, hmeq⟩ := rust_erase_term_refines .Nil t
  refine ⟨m, hm, ?_⟩
  rw [hmeq]
  exact specEraseTerm_simulation ht hsteps

/-! ## Part 2: TEGG v15 Core (`tir_cdsl_core::tegg_core` ⊨ `CdslV3Spec.TeggSpec`) -/

def fromRustNode {T : Type} (n : tegg_core.VmNode T) : CdslV3Spec.VmNode T :=
  { payload := n.payload, children := n.children.val.map (·.val) }

def fromRustColReq : tegg_core.ColReq → CdslV3Spec.ColReq
  | .Fresh => .fresh
  | .Known r => .known r.val

def fromRustBind : tegg_core.Bind → CdslV3Spec.Bind
  | .New r => .new r.val
  | .Same r => .same r.val

private def specStringEntryBytes (len : Nat) : Nat :=
  4 + len + (4 - len % 4) % 4

private def specSumStringBytes : List Usize → Nat
  | [] => 0
  | l :: rest => specStringEntryBytes l.val + specSumStringBytes rest

private theorem specStringEntryBytes_mod4 (len : Nat) : specStringEntryBytes len % 4 = 0 := by
  unfold specStringEntryBytes
  omega

private theorem specSumStringBytes_mod4 (lens : List Usize) : specSumStringBytes lens % 4 = 0 := by
  induction lens with
  | nil => rfl
  | cons l rest ih =>
    simp only [specSumStringBytes]
    have h1 := specStringEntryBytes_mod4 l.val
    omega

private theorem specSumStringBytes_drop_step (lens : List Usize) (i : Nat) (hi : i < lens.length) :
    specSumStringBytes (lens.drop i) =
      specStringEntryBytes lens[i].val + specSumStringBytes (lens.drop (i + 1)) := by
  rw [List.drop_eq_getElem_cons hi]
  rfl

/-- **Theorem 2a (TEGG `pad4` Exactness & Panic-Freedom)** -/
theorem rust_pad4_spec (len : Usize) :
    ∃ r : Usize, tegg_core.pad4 len = ok r ∧ r.val = (4 - len.val % 4) % 4 := by
  unfold tegg_core.pad4
  have h4 : (4#usize : Usize).val = 4 := rfl
  obtain ⟨r1, hr1, hv1⟩ := usize_rem_ok len 4#usize (by omega)
  have hlt4 : r1.val < 4 := by
    rw [hv1, h4]
    exact Nat.mod_lt _ (by decide)
  obtain ⟨r2, hr2, hv2⟩ := usize_sub_ok 4#usize r1 (by omega)
  obtain ⟨r3, hr3, hv3⟩ := usize_rem_ok r2 4#usize (by omega)
  refine ⟨r3, ?_, ?_⟩
  · simp [hr1, hr2, hr3, Aeneas.Std.bind_ok]
  · rw [hv3, hv2, hv1, h4]

/-- **Theorem 2b (TEGG Validator Invariant #3a: Per-String 4-Byte Alignment)** -/
theorem rust_string_entry_bytes_aligned (len : Usize)
    (hbound : specStringEntryBytes len.val ≤ Usize.max) :
    ∃ out : Usize, tegg_core.string_entry_bytes len = ok out ∧
      out.val = specStringEntryBytes len.val ∧ out.val % 4 = 0 := by
  unfold tegg_core.string_entry_bytes
  obtain ⟨p, hp, hpv⟩ := rust_pad4_spec len
  have h4 : (4#usize : Usize).val = 4 := rfl
  have hsum1 : (4#usize : Usize).val + len.val ≤ Usize.max := by
    unfold specStringEntryBytes at hbound
    omega
  obtain ⟨s1, hs1, hs1v⟩ := usize_add_ok 4#usize len hsum1
  have hsum2 : s1.val + p.val ≤ Usize.max := by
    unfold specStringEntryBytes at hbound
    omega
  obtain ⟨s2, hs2, hs2v⟩ := usize_add_ok s1 p hsum2
  refine ⟨s2, ?_, ?_, ?_⟩
  · simp [hs1, hp, hs2, Aeneas.Std.bind_ok]
  · unfold specStringEntryBytes
    omega
  · unfold specStringEntryBytes at *
    omega

/-- **Theorem 2c (TEGG Validator Invariant #3b: Full File 4-Byte Alignment)** -/
theorem rust_sum_string_entry_bytes_spec (lens : Slice Usize)
    (hbound : specSumStringBytes lens.val ≤ Usize.max) :
    ∃ total : Usize, tegg_core.sum_string_entry_bytes lens = ok total ∧
      total.val = specSumStringBytes lens.val ∧ total.val % 4 = 0 := by
  unfold tegg_core.sum_string_entry_bytes tegg_core.sum_string_entry_bytes_loop
  let inv : Usize × Usize → Prop := fun (tot, i) =>
    i.val ≤ lens.val.length ∧
    tot.val + specSumStringBytes (lens.val.drop i.val) = specSumStringBytes lens.val
  let post : Usize → Prop := fun tot =>
    tot.val = specSumStringBytes lens.val ∧ tot.val % 4 = 0
  have hwp : WP.spec (loop (fun (tot, i) => tegg_core.sum_string_entry_bytes_loop.body lens tot i)
      (0#usize, 0#usize)) post := by
    apply loop.spec_decr_nat (fun (_, i) => lens.val.length - i.val) inv post
    · intro ⟨tot, i⟩ ⟨hle, hsum⟩
      unfold tegg_core.sum_string_entry_bytes_loop.body
      obtain ⟨n, hlen, hn⟩ := slice_len_ok lens
      rw [hlen]
      simp only [Aeneas.Std.bind_ok]
      by_cases hlt : i.val < lens.val.length
      · have hcmp : i < n := by change i.val < n.val; omega
        simp only [hcmp, ↓reduceIte]
        have hidx : Slice.index_usize lens i = ok lens.val[i.val] := by
          simp [Slice.index_usize, hlt]
        rw [hidx]
        simp only [Aeneas.Std.bind_ok]
        have hstep := specSumStringBytes_drop_step lens.val i.val hlt
        have heb : specStringEntryBytes lens.val[i.val].val ≤ Usize.max := by omega
        obtain ⟨eb, heb_ok, heb_val, _⟩ := rust_string_entry_bytes_aligned lens.val[i.val] heb
        rw [heb_ok]
        simp only [Aeneas.Std.bind_ok]
        obtain ⟨tot1, htot1_ok, htot1_val⟩ := usize_add_ok tot eb (by omega)
        rw [htot1_ok]
        simp only [Aeneas.Std.bind_ok]
        have h1 : (1#usize : Usize).val = 1 := rfl
        have hi_bound : i.val + (1#usize : Usize).val ≤ Usize.max := by
          have := lens.property
          omega
        obtain ⟨i1, hi1_ok, hi1_val⟩ := usize_add_ok i 1#usize hi_bound
        rw [hi1_ok]
        simp only [Aeneas.Std.bind_ok]
        apply ok_to_wp
        dsimp only
        have hi1_eq : i1.val = i.val + 1 := by omega
        refine ⟨⟨by omega, ?_⟩, by omega⟩
        rw [hi1_eq]
        omega
      · have hcmp : ¬ (i < n) := by change ¬ (i.val < n.val); omega
        simp only [hcmp, ↓reduceIte]
        apply ok_to_wp
        have heq_len : i.val = lens.val.length := by omega
        rw [heq_len, List.drop_length] at hsum
        simp only [specSumStringBytes, Nat.add_zero] at hsum
        exact ⟨hsum, by rw [hsum]; exact specSumStringBytes_mod4 lens.val⟩
    · exact ⟨Nat.zero_le _, by simp⟩
  obtain ⟨tot, hok, hval, hmod⟩ := wp_to_ok hwp
  exact ⟨tot, hok, hval, hmod⟩

theorem rust_tegg_file_bytes_aligned (str_lens : Slice Usize) (word_count : Usize)
    (hbound : 12 + specSumStringBytes str_lens.val + 4 * word_count.val ≤ Usize.max) :
    ∃ out : Usize, tegg_core.tegg_file_bytes str_lens word_count = ok out ∧ out.val % 4 = 0 := by
  unfold tegg_core.tegg_file_bytes
  have h12 : (12#usize : Usize).val = 12 := rfl
  have h4 : (4#usize : Usize).val = 4 := rfl
  obtain ⟨sum, hsum_ok, hsum_val, hsum_mod⟩ := rust_sum_string_entry_bytes_spec str_lens (by omega)
  obtain ⟨s1, hs1_ok, hs1_val⟩ := usize_add_ok 12#usize sum (by rw [h12]; omega)
  obtain ⟨w4, hw4_ok, hw4_val⟩ := usize_mul_ok 4#usize word_count (by rw [h4]; omega)
  obtain ⟨s2, hs2_ok, hs2_val⟩ := usize_add_ok s1 w4 (by rw [hs1_val, hw4_val, h12, h4]; omega)
  refine ⟨s2, ?_, ?_⟩
  · simp [hsum_ok, hs1_ok, hw4_ok, hs2_ok, Aeneas.Std.bind_ok]
  · rw [hs2_val, hs1_val, hw4_val, h12, h4]
    omega

/-- **Theorem 2d (TEGG Validator Invariant #2: Sequential Register Binding)** -/
private theorem checkBinds_append (w : Nat) (bs1 bs2 : List CdslV3Spec.Bind) (w1 : Nat)
    (h1 : checkBinds w bs1 = some w1) :
    checkBinds w (bs1 ++ bs2) = checkBinds w1 bs2 := by
  induction bs1 generalizing w with
  | nil =>
    simp [checkBinds] at h1 ⊢
    subst h1; rfl
  | cons b rest ih =>
    cases b with
    | new r =>
      simp only [List.cons_append, checkBinds] at h1 ⊢
      split at h1
      · rename_i heq
        simp only [heq, ↓reduceIte]
        exact ih (w + 1) h1
      · contradiction
    | same r =>
      simp only [List.cons_append, checkBinds] at h1 ⊢
      split at h1
      · rename_i hlt
        simp only [hlt, ↓reduceIte]
        exact ih w h1
      · contradiction

theorem rust_alloc_binds_valid (w : Usize) (reqs : Slice tegg_core.ColReq)
    (hbound : w.val + reqs.val.length ≤ Usize.max)
    (hvalid : ValidReqs w.val (reqs.val.map fromRustColReq)) :
    ∃ binds w', tegg_core.alloc_binds w reqs = ok (binds, w') ∧
      checkBinds w.val (binds.val.map fromRustBind) = some w'.val := by
  unfold tegg_core.alloc_binds tegg_core.alloc_binds_loop
  obtain ⟨empty_binds, hnew, hempty_val⟩ := vec_new_ok tegg_core.Bind
  rw [hnew]
  simp only [Aeneas.Std.bind_ok]
  let inv : Usize × alloc.vec.Vec tegg_core.Bind × Usize → Prop :=
    fun (cur_w, cur_binds, i) =>
      i.val ≤ reqs.val.length ∧
      cur_binds.val.length = i.val ∧
      cur_w.val ≤ w.val + i.val ∧
      checkBinds w.val (cur_binds.val.map fromRustBind) = some cur_w.val ∧
      ValidReqs cur_w.val ((reqs.val.drop i.val).map fromRustColReq)
  let post : alloc.vec.Vec tegg_core.Bind × Usize → Prop :=
    fun (binds, w') => checkBinds w.val (binds.val.map fromRustBind) = some w'.val
  have hwp : WP.spec (loop (fun (w1, b1, i1) => tegg_core.alloc_binds_loop.body reqs w1 b1 i1)
      (w, empty_binds, 0#usize)) post := by
    apply loop.spec_decr_nat (fun (_, _, i) => reqs.val.length - i.val) inv post
    · intro ⟨cur_w, cur_binds, i⟩ ⟨hle, hblen, hwle, hchk, hvr⟩
      unfold tegg_core.alloc_binds_loop.body
      obtain ⟨n, hlen, hn⟩ := slice_len_ok reqs
      rw [hlen]
      simp only [Aeneas.Std.bind_ok]
      by_cases hlt : i.val < reqs.val.length
      · have hcmp : i < n := by change i.val < n.val; omega
        simp only [hcmp, ↓reduceIte]
        have hidx : Slice.index_usize reqs i = ok reqs.val[i.val] := by
          simp [Slice.index_usize, hlt]
        rw [hidx]
        simp only [Aeneas.Std.bind_ok]
        have hdrop : reqs.val.drop i.val = reqs.val[i.val] :: reqs.val.drop (i.val + 1) :=
          List.drop_eq_getElem_cons hlt
        rw [hdrop, List.map_cons] at hvr
        have h1 : (1#usize : Usize).val = 1 := rfl
        have hi_bound : i.val + (1#usize : Usize).val ≤ Usize.max := by have := reqs.property; omega
        obtain ⟨i2, hi2_ok, hi2_val⟩ := usize_add_ok i 1#usize hi_bound
        cases hreq : reqs.val[i.val] with
        | Fresh =>
          rw [hreq] at hvr
          simp only [fromRustColReq, ValidReqs] at hvr
          dsimp only
          obtain ⟨binds2, hpush, hbinds2_val⟩ :=
            vec_push_ok cur_binds (.New cur_w) (by have := reqs.property; omega)
          rw [hpush]
          simp only [Aeneas.Std.bind_ok]
          obtain ⟨w2, hw2_ok, hw2_val⟩ := usize_add_ok cur_w 1#usize (by omega)
          rw [hw2_ok]
          simp only [Aeneas.Std.bind_ok]
          rw [hi2_ok]
          simp only [Aeneas.Std.bind_ok]
          apply ok_to_wp
          dsimp only
          have hi2_eq : i2.val = i.val + 1 := by omega
          have hw2_eq : w2.val = cur_w.val + 1 := by omega
          refine ⟨⟨by omega, by rw [hbinds2_val]; simp [hblen, hi2_eq], by omega, ?_, ?_⟩, by omega⟩
          · rw [hbinds2_val, List.map_append, List.map_singleton]
            rw [checkBinds_append _ _ _ _ hchk]
            simp [fromRustBind, checkBinds, hw2_eq]
          · simpa [hi2_eq, hw2_eq] using hvr
        | Known r =>
          rw [hreq] at hvr
          simp only [fromRustColReq, ValidReqs] at hvr
          rcases hvr with ⟨hr_lt, hvr_rest⟩
          dsimp only
          obtain ⟨binds2, hpush, hbinds2_val⟩ :=
            vec_push_ok cur_binds (.Same r) (by have := reqs.property; omega)
          rw [hpush]
          simp only [Aeneas.Std.bind_ok]
          rw [hi2_ok]
          simp only [Aeneas.Std.bind_ok]
          apply ok_to_wp
          dsimp only
          have hi2_eq : i2.val = i.val + 1 := by omega
          refine ⟨⟨by omega, by rw [hbinds2_val]; simp [hblen, hi2_eq], by omega, ?_, ?_⟩, by omega⟩
          · rw [hbinds2_val, List.map_append, List.map_singleton]
            rw [checkBinds_append _ _ _ _ hchk]
            simp [fromRustBind, checkBinds, hr_lt]
          · simpa [hi2_eq] using hvr_rest
      · have hcmp : ¬ (i < n) := by change ¬ (i.val < n.val); omega
        simp only [hcmp, ↓reduceIte]
        exact ok_to_wp hchk
    · refine ⟨Nat.zero_le _, by simp [hempty_val], Nat.le_refl _, ?_, by simpa using hvalid⟩
      simp [hempty_val, checkBinds]
  obtain ⟨⟨binds, w'⟩, hok, hpost⟩ := wp_to_ok hwp
  exact ⟨binds, w', hok, hpost⟩

/-- **Theorem 2e (TEGG Validator Invariant #1: Bottom-Up DAG Child Ordering)** -/
private theorem rust_push_parent_node_valid {T : Type}
    (nodes : alloc.vec.Vec (tegg_core.VmNode T)) (child_id : Usize) (step : T)
    (hbound : nodes.val.length + 1 ≤ Usize.max)
    (hvalid : ValidChildOrder (nodes.val.map fromRustNode))
    (hchild : child_id.val < nodes.val.length) :
    ∃ parent_id nodes', tegg_core.push_parent_node nodes child_id step = ok (parent_id, nodes') ∧
      nodes'.val.length = nodes.val.length + 1 ∧
      parent_id.val = nodes.val.length ∧
      ValidChildOrder (nodes'.val.map fromRustNode) := by
  unfold tegg_core.push_parent_node
  obtain ⟨pid, hlen, hpid⟩ := vec_len_ok nodes
  obtain ⟨c0, hc0, hc0_val⟩ := vec_new_ok Usize
  obtain ⟨c1, hc1, hc1_val⟩ := vec_push_ok c0 child_id (by rw [hc0_val]; simp; scalar_tac)
  let pnode : tegg_core.VmNode T := { payload := step, children := c1 }
  obtain ⟨nodes1, hpush_n, hnodes1_val⟩ := vec_push_ok nodes pnode hbound
  refine ⟨pid, nodes1, ?_, ?_, hpid, ?_⟩
  · simp [hlen, hc0, hc1, hpush_n, pnode, Aeneas.Std.bind_ok]
  · simp [hnodes1_val]
  · rw [hnodes1_val, List.map_append, List.map_singleton]
    apply validChildOrder_append_one hvalid
    intro c hc
    simp [fromRustNode, pnode, hc1_val, hc0_val] at hc
    subst hc
    simpa [List.length_map] using hchild

private def vecLen {T : Type} (v : alloc.vec.Vec T) : Nat := v.val.length

private theorem build_chain_body_step {T : Type}
    (st : alloc.vec.Vec T) (ns : alloc.vec.Vec (tegg_core.VmNode T)) (cid : Usize)
    (hb : vecLen ns + vecLen st ≤ Usize.max)
    (hv : ValidChildOrder (ns.val.map fromRustNode))
    (hc : cid.val < vecLen ns) :
    (0 < vecLen st →
      ∃ st1 ns1 pid,
        tegg_core.build_chain_aux_loop.body st ns cid = ok (ControlFlow.cont (st1, ns1, pid)) ∧
        vecLen ns1 + vecLen st1 ≤ Usize.max ∧
        ValidChildOrder (ns1.val.map fromRustNode) ∧
        pid.val < vecLen ns1 ∧
        vecLen st1 < vecLen st) ∧
    (vecLen st = 0 →
      tegg_core.build_chain_aux_loop.body st ns cid = ok (ControlFlow.done (ns, cid))) := by
  simp only [vecLen] at hb hc ⊢
  constructor
  · intro hpos
    unfold tegg_core.build_chain_aux_loop.body
    obtain ⟨elem, st1, hpop, hst1_len⟩ := (vec_pop_ok st).2 hpos
    obtain ⟨pid, ns1, hpp, hns1_len, hpid_val, hns1_valid⟩ :=
      rust_push_parent_node_valid ns cid elem (by omega) hv hc
    refine ⟨st1, ns1, pid, ?_, by omega, hns1_valid, by omega, by omega⟩
    rw [hpop]
    simp only [Aeneas.Std.bind_ok]
    change (do
      let (child_id1, nodes1) ← tegg_core.push_parent_node ns cid elem
      ok (ControlFlow.cont (st1, nodes1, child_id1))) = _
    rw [hpp]
    simp
  · intro hzero
    unfold tegg_core.build_chain_aux_loop.body
    have hpop := (vec_pop_ok st).1 hzero
    rw [hpop]
    simp only [Aeneas.Std.bind_ok]
    rfl

theorem rust_build_chain_valid {T : Type}
    (nodes : alloc.vec.Vec (tegg_core.VmNode T)) (leaf_step : T) (parent_steps : alloc.vec.Vec T)
    (hbound : nodes.val.length + 1 + parent_steps.val.length ≤ Usize.max)
    (hvalid : ValidChildOrder (nodes.val.map fromRustNode)) :
    ∃ nodes' root, tegg_core.build_chain nodes leaf_step parent_steps = ok (nodes', root) ∧
      ValidChildOrder (nodes'.val.map fromRustNode) ∧
      root.val < nodes'.val.length := by
  unfold tegg_core.build_chain tegg_core.build_chain_aux tegg_core.build_chain_aux_loop
  obtain ⟨leaf_id, hlen, hleaf_val⟩ := vec_len_ok nodes
  obtain ⟨empty_c, hnew, hempty_c⟩ := vec_new_ok Usize
  let lnode : tegg_core.VmNode T := { payload := leaf_step, children := empty_c }
  obtain ⟨nodes1, hpush_l, hnodes1_val⟩ := vec_push_ok nodes lnode (by omega)
  have hvalid1 : ValidChildOrder (nodes1.val.map fromRustNode) := by
    rw [hnodes1_val, List.map_append, List.map_singleton]
    apply validChildOrder_append_one hvalid
    intro c hc
    simp [fromRustNode, lnode, hempty_c] at hc
  have hdm_ex : ∃ (s : Slice T) (back : Slice T → alloc.vec.Vec T),
      alloc.vec.Vec.Insts.CoreOpsDerefDerefMutSlice.deref_mut parent_steps = ok (s, back) ∧
      s.val = parent_steps.val ∧ ∀ x, (back x).val = x.val := by
    refine ⟨parent_steps, fun x => x, ?_, rfl, fun _ => rfl⟩
    simp only [alloc.vec.Vec.Insts.CoreOpsDerefDerefMutSlice.deref_mut,
      alloc.vec.Vec.as_mut_slice, rust_primitives.sequence.seq_to_slice_mut, Aeneas.Std.bind_ok]
    rfl
  obtain ⟨s, back, hdm, hs_val, hback_val⟩ := hdm_ex
  obtain ⟨rev_steps, hrev, hrev_val⟩ := slice_reverse_ok s
  let inv : alloc.vec.Vec T × alloc.vec.Vec (tegg_core.VmNode T) × Usize → Prop :=
    fun (st, ns, cid) =>
      vecLen ns + vecLen st ≤ Usize.max ∧
      ValidChildOrder (ns.val.map fromRustNode) ∧
      cid.val < vecLen ns
  let post : alloc.vec.Vec (tegg_core.VmNode T) × Usize → Prop :=
    fun (ns, root) => ValidChildOrder (ns.val.map fromRustNode) ∧ root.val < vecLen ns
  have hwp : WP.spec (loop (fun (s1, n1, c1) => tegg_core.build_chain_aux_loop.body s1 n1 c1)
      (back rev_steps, nodes1, leaf_id)) post := by
    apply loop.spec_decr_nat (fun x => vecLen x.1) inv post
    · intro ⟨st, ns, cid⟩ ⟨hb, hv, hc⟩
      have hstep := build_chain_body_step st ns cid hb hv hc
      by_cases hpos : 0 < vecLen st
      · obtain ⟨st1, ns1, pid, heq, hb1, hv1, hc1, hlt⟩ := hstep.1 hpos
        change WP.spec (tegg_core.build_chain_aux_loop.body st ns cid) _
        rw [heq]
        exact ok_to_wp ⟨⟨hb1, hv1, hc1⟩, hlt⟩
      · have heq := hstep.2 (by omega)
        change WP.spec (tegg_core.build_chain_aux_loop.body st ns cid) _
        rw [heq]
        exact ok_to_wp ⟨hv, hc⟩
    · refine ⟨?_, hvalid1, ?_⟩
      · simp [vecLen, hnodes1_val, hback_val, hrev_val, hs_val]; omega
      · simp [vecLen, hnodes1_val, hleaf_val]
  obtain ⟨⟨ns', root⟩, hok, hv', hr'⟩ := wp_to_ok hwp
  refine ⟨ns', root, ?_, hv', hr'⟩
  simp only [hlen, hnew, Aeneas.Std.bind_ok]
  change (do
    let nodes1 ← nodes.push lnode
    let (s, deref_mut_back) ← alloc.vec.Vec.Insts.CoreOpsDerefDerefMutSlice.deref_mut parent_steps
    let s1 ← core.slice.Slice.reverse s
    loop (fun (s1, n1, c1) => tegg_core.build_chain_aux_loop.body (T := T) s1 n1 c1) (deref_mut_back s1, nodes1, leaf_id)) = _
  rw [hpush_l]
  simp only [Bind.bind, Aeneas.Std.bind_ok]
  rw [hdm]
  simp only [Aeneas.Std.bind_ok]
  change (do
    let s1 ← core.slice.Slice.reverse s
    loop (fun (s1, n1, c1) => tegg_core.build_chain_aux_loop.body (T := T) s1 n1 c1) (back s1, nodes1, leaf_id)) = _
  rw [hrev]
  simp only [Bind.bind, Aeneas.Std.bind_ok]
  exact hok

end CdslV3.Verification

/-! ## Axiom Audits (ZERO `sorryAx`, ZERO custom axioms) -/

set_option pp.fullNames true

/--
info: 'CdslV3.Verification.rust_prop_elim_guard_sound' depends on axioms: [propext, Classical.choice, Quot.sound]
-/
#guard_msgs in
#print axioms CdslV3.Verification.rust_prop_elim_guard_sound

/--
info: 'CdslV3.Verification.rust_infer_type_sound' depends on axioms: [propext, Classical.choice, Quot.sound]
-/
#guard_msgs in
#print axioms CdslV3.Verification.rust_infer_type_sound

/--
info: 'CdslV3.Verification.rust_infer_type_complete' depends on axioms: [propext, Classical.choice, Quot.sound]
-/
#guard_msgs in
#print axioms CdslV3.Verification.rust_infer_type_complete

/--
info: 'CdslV3.Verification.rust_erase_prop' depends on axioms: [propext, Classical.choice, Quot.sound]
-/
#guard_msgs in
#print axioms CdslV3.Verification.rust_erase_prop

/--
info: 'CdslV3.Verification.rust_erase_simulation' depends on axioms: [propext, Classical.choice, Quot.sound]
-/
#guard_msgs in
#print axioms CdslV3.Verification.rust_erase_simulation

/--
info: 'CdslV3.Verification.rust_erase_preserves_int' depends on axioms: [propext, Classical.choice, Quot.sound]
-/
#guard_msgs in
#print axioms CdslV3.Verification.rust_erase_preserves_int

/--
info: 'CdslV3.Verification.rust_erase_preserves_bool' depends on axioms: [propext, Classical.choice, Quot.sound]
-/
#guard_msgs in
#print axioms CdslV3.Verification.rust_erase_preserves_bool

/--
info: 'CdslV3.Verification.rust_string_entry_bytes_aligned' depends on axioms: [propext, Classical.choice, Quot.sound]
-/
#guard_msgs in
#print axioms CdslV3.Verification.rust_string_entry_bytes_aligned

/--
info: 'CdslV3.Verification.rust_tegg_file_bytes_aligned' depends on axioms: [propext, Classical.choice, Quot.sound]
-/
#guard_msgs in
#print axioms CdslV3.Verification.rust_tegg_file_bytes_aligned

/--
info: 'CdslV3.Verification.rust_alloc_binds_valid' depends on axioms: [propext, Classical.choice, Quot.sound]
-/
#guard_msgs in
#print axioms CdslV3.Verification.rust_alloc_binds_valid

/--
info: 'CdslV3.Verification.rust_build_chain_valid' depends on axioms: [propext, Classical.choice, Quot.sound]
-/
#guard_msgs in
#print axioms CdslV3.Verification.rust_build_chain_valid
