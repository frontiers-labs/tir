import Hm.Verification.ProofObligations

open CoreModels Aeneas
open Aeneas.Std hiding namespace core alloc
open RustM
open cdsl_hm

namespace CDSL.HM.Refinement

def lookup (xs : List (VarId × cdsl_hm.Ty)) (v : VarId) : cdsl_hm.Ty :=
  match xs with
  | [] => .Var v
  | (w,t) :: rest => if w = v then t else lookup rest v

theorem next_nil {α : Type} (xs : core.slice.iter.Iter α) (h : xs.val = []) :
    core.slice.iter.Iter.Insts.CoreIterTraitsIteratorIteratorSharedAT.next xs =
      ok (.None, xs) := by
  have len : Slice.len xs = 0#usize := by scalar_tac
  simp [core.slice.iter.Iter.Insts.CoreIterTraitsIteratorIteratorSharedAT.next,
    rust_primitives.sequence.seq_len, bind_ok, len]

theorem next_cons {α : Type} (xs : core.slice.iter.Iter α) (x : α) (rest : List α)
    (h : xs.val = x :: rest) :
    ∃ tail : core.slice.iter.Iter α,
      core.slice.iter.Iter.Insts.CoreIterTraitsIteratorIteratorSharedAT.next xs =
        ok (.Some x, tail) ∧ tail.val = rest := by
  have len : Slice.len xs ≠ 0#usize := by scalar_tac
  unfold core.slice.iter.Iter.Insts.CoreIterTraitsIteratorIteratorSharedAT.next
    rust_primitives.sequence.seq_len
  simp only [bind_ok, len, ↓reduceIte]
  unfold rust_primitives.sequence.seq_remove
  simp [h, bind_ok]
  refine ⟨_, rfl, ?_⟩
  simp

theorem lookup_refines (xs : core.slice.iter.Iter (VarId × cdsl_hm.Ty)) (v : VarId) :
    cdsl_hm.Substitution.apply_loop xs v = ok (lookup xs.val v) := by
  unfold cdsl_hm.Substitution.apply_loop
  rw [loop.eq_def]
  cases hxs : xs.val with
  | nil =>
    have body : Substitution.apply_loop.body v xs = ok (.done (.Var v)) := by
      simp [Substitution.apply_loop.body, next_nil xs hxs, bind_ok,
        Ty.Insts.CoreCloneClone.clone, clone_type_refines]
    rw [body, bind_ok]
    simp [lookup]
  | cons head rest =>
    obtain ⟨tail, hn, ht⟩ := next_cons xs head rest hxs
    rcases head with ⟨w,t⟩
    have body : Substitution.apply_loop.body v xs =
        ok (if w = v then .done t else .cont tail) := by
      simp [Substitution.apply_loop.body, hn, bind_ok,
        VarId.Insts.CoreCmpPartialEqVarId.eq, Ty.Insts.CoreCloneClone.clone,
        clone_type_refines]
      split <;> rfl
    rw [body, bind_ok]
    by_cases eq : w = v
    · simp [eq, lookup]
    · simp only [eq, ↓reduceIte]
      change Substitution.apply_loop tail v = _
      rw [lookup_refines tail v]
      simp [lookup, eq, ht]
termination_by xs.val.length
decreasing_by simp_all
mutual
  def substitute (s : VarId → cdsl_hm.Ty) : cdsl_hm.Ty → cdsl_hm.Ty
    | .Var v => s v
    | .Rigid v => .Rigid v
    | .Con c args => .Con c (substituteList s args)
    | .Fun args result => .Fun (substituteList s args) (substitute s result)
  def substituteList (s : VarId → cdsl_hm.Ty) : TyList → TyList
    | .Nil => .Nil
    | .Cons t rest => .Cons (substitute s t) (substituteList s rest)
end

theorem into_iter_refines {α : Type} (xs : alloc.vec.Vec α) :
    alloc.SharedAVec.Insts.CoreIterTraitsCollectIntoIteratorSharedATIter.into_iter xs = ok xs := by
  simp [alloc.SharedAVec.Insts.CoreIterTraitsCollectIntoIteratorSharedATIter.into_iter,
    alloc.vec.Vec.as_slice, core.slice.Slice.iter, rust_primitives.sequence.seq_to_slice,
    rust_primitives.sequence.seq_from_slice]
  exact bind_ok _ _

mutual
  theorem apply_refines (s : cdsl_hm.Substitution) (t : cdsl_hm.Ty) :
      s.apply t = ok (substitute (lookup s.bindings.val) t) := by
    cases t with
    | Var v =>
      rw [Substitution.apply]
      rw [into_iter_refines, bind_ok, lookup_refines]
      rfl
    | Rigid v => simp [Substitution.apply, Ty.Insts.CoreCloneClone.clone,
        clone_type_refines, substitute]
    | Con c args =>
      rw [Substitution.apply, apply_list_refines, bind_ok]
      rfl
    | Fun args result =>
      rw [Substitution.apply, apply_list_refines, bind_ok, apply_refines, bind_ok]
      rfl
  termination_by sizeOf t

  theorem apply_list_refines (s : cdsl_hm.Substitution) (ts : TyList) :
      s.apply_list ts = ok (substituteList (lookup s.bindings.val) ts) := by
    cases ts with
    | Nil => rw [Substitution.apply_list]; rfl
    | Cons t rest =>
      rw [Substitution.apply_list, apply_refines, bind_ok, apply_list_refines, bind_ok]
      rfl
  termination_by sizeOf ts
end

mutual
  def decodeType : cdsl_hm.Ty → CDSL.HM.Ty
    | .Var v => .var v.val
    | .Rigid v => .rigid v.val
    | .Con c args => .app (.atom (c.val + 3)) (decodeTypes args)
    | .Fun args result => .app (.app (.atom 0) (decodeTypes args)) (decodeType result)
  def decodeTypes : TyList → CDSL.HM.Ty
    | .Nil => .atom 1
    | .Cons t rest => .app (.app (.atom 2) (decodeType t)) (decodeTypes rest)
end

def decodeSubstitution (xs : List (VarId × cdsl_hm.Ty)) (v : Nat) : CDSL.HM.Ty :=
  match xs with
  | [] => .var v
  | (w,t) :: rest => if w.val = v then decodeType t else decodeSubstitution rest v

theorem lookup_decodes (xs : List (VarId × cdsl_hm.Ty)) (v : VarId) :
    decodeType (lookup xs v) = decodeSubstitution xs v.val := by
  induction xs with
  | nil => rfl
  | cons head rest ih =>
    rcases head with ⟨w,t⟩
    have eqv : w.val = v.val ↔ w = v :=
      ⟨UScalar.eq_of_val_eq, fun h => congrArg UScalar.val h⟩
    simp only [lookup, decodeSubstitution, eqv]
    split <;> simp_all

mutual
  theorem substitution_decodes (s : VarId → cdsl_hm.Ty) (σ : CDSL.HM.Substitution)
      (agree : ∀ v, decodeType (s v) = σ v.val) (t : cdsl_hm.Ty) :
      decodeType (substitute s t) = (decodeType t).subst σ := by
    cases t with
    | Var v => exact agree v
    | Rigid v => rfl
    | Con c args =>
      simp [substitute, decodeType, CDSL.HM.Ty.subst, substitutions_decode s σ agree args]
    | Fun args result =>
      simp [substitute, decodeType, CDSL.HM.Ty.subst,
        substitutions_decode s σ agree args, substitution_decodes s σ agree result]
  termination_by sizeOf t

  theorem substitutions_decode (s : VarId → cdsl_hm.Ty) (σ : CDSL.HM.Substitution)
      (agree : ∀ v, decodeType (s v) = σ v.val) (ts : TyList) :
      decodeTypes (substituteList s ts) = (decodeTypes ts).subst σ := by
    cases ts with
    | Nil => rfl
    | Cons t rest =>
      simp [substituteList, decodeTypes, CDSL.HM.Ty.subst,
        substitution_decodes s σ agree t, substitutions_decode s σ agree rest]
  termination_by sizeOf ts
end

/-- Production substitution implements simultaneous substitution in the independent specification. -/
theorem apply_semantic_refinement (s : cdsl_hm.Substitution) (t : cdsl_hm.Ty) :
    ∃ result, s.apply t = ok result ∧
      decodeType result = (decodeType t).subst (decodeSubstitution s.bindings.val) := by
  refine ⟨substitute (lookup s.bindings.val) t, apply_refines s t, ?_⟩
  exact substitution_decodes _ _ (lookup_decodes s.bindings.val) t

theorem apply_list_semantic_refinement (s : cdsl_hm.Substitution) (ts : TyList) :
    ∃ result, s.apply_list ts = ok result ∧
      decodeTypes result = (decodeTypes ts).subst (decodeSubstitution s.bindings.val) := by
  refine ⟨substituteList (lookup s.bindings.val) ts, apply_list_refines s ts, ?_⟩
  exact substitutions_decode _ _ (lookup_decodes s.bindings.val) ts

end CDSL.HM.Refinement
