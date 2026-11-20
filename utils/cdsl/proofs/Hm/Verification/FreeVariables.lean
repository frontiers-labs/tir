import Hm.Verification.Composition

open CoreModels Aeneas
open Aeneas.Std hiding namespace core alloc
open RustM cdsl_hm
set_option backward.isDefEq.respectTransparency false

namespace CDSL.HM.Refinement

mutual
  def freeType : cdsl_hm.Ty → List VarId
    | .Var v => [v]
    | .Rigid _ => []
    | .Con _ args => freeTypes args
    | .Fun args result => freeTypes args ++ freeType result
  def freeTypes : TyList → List VarId
    | .Nil => []
    | .Cons t rest => freeType t ++ freeTypes rest
end

theorem contains_list_refines (xs : List VarId) (v : VarId) :
    rust_primitives.slice.slice_contains_go VarId.Insts.CoreCmpPartialEqVarId v xs =
      ok (decide (v ∈ xs)) := by
  induction xs with
  | nil => rfl
  | cons w rest ih =>
    rw [rust_primitives.slice.slice_contains_go]
    change (do
      let b ← VarId.Insts.CoreCmpPartialEqVarId.eq w v
      if b then ok true else rust_primitives.slice.slice_contains_go VarId.Insts.CoreCmpPartialEqVarId v rest) = _
    rw [VarId.Insts.CoreCmpPartialEqVarId.eq]
    simp only [bind_tc_ok]
    by_cases eq : w = v
    · subst w; simp
    · have ne : v ≠ w := Ne.symm eq
      simp [eq, ne, ih]

theorem contains_refines (xs : Slice VarId) (v : VarId) :
    core.slice.Slice.contains VarId.Insts.CoreCmpPartialEqVarId xs v = ok (decide (v ∈ xs.val)) :=
  contains_list_refines xs.val v

theorem deref_refines {α : Type} (xs : alloc.vec.Vec α) :
    alloc.vec.Vec.Insts.CoreOpsDerefDerefSlice.deref xs = ok xs := by
  simp [alloc.vec.Vec.Insts.CoreOpsDerefDerefSlice.deref, alloc.vec.Vec.as_slice,
    rust_primitives.sequence.seq_to_slice]

mutual
  theorem collect_free_refines (t : cdsl_hm.Ty) (acc : alloc.vec.Vec VarId) :
      Returns (collect_free t acc) (fun out => ∀ v, v ∈ out.val ↔ v ∈ acc.val ∨ v ∈ freeType t) := by
    intro out h v
    cases t with
    | Var w =>
      rw [collect_free, deref_refines, bind_ok, contains_refines, bind_ok] at h
      by_cases present : w ∈ acc.val
      · simp only [present, decide_true, ↓reduceIte] at h
        have eq := RustM.ok_injective h
        subst out
        simp [freeType]
        intro eq; subst v; exact present
      · simp only [present, decide_false, Bool.false_eq_true, ↓reduceIte] at h
        have hp := push_success acc w out h
        simp [hp, freeType]
    | Rigid w =>
      rw [collect_free] at h
      have eq := RustM.ok_injective h
      subst out
      simp [freeType]
    | Con c args => exact collect_free_list_refines args acc out (by simpa only [collect_free] using h) v
    | Fun args result =>
      rw [collect_free] at h
      obtain ⟨middle, hm, ho⟩ := bind_success _ _ _ h
      rw [collect_free_refines result middle out ho v,
        collect_free_list_refines args acc middle hm v]
      simp [freeType, or_assoc]
  termination_by sizeOf t

  theorem collect_free_list_refines (ts : TyList) (acc : alloc.vec.Vec VarId) :
      Returns (collect_free_list ts acc) (fun out => ∀ v, v ∈ out.val ↔ v ∈ acc.val ∨ v ∈ freeTypes ts) := by
    intro out h v
    cases ts with
    | Nil =>
      rw [collect_free_list] at h
      have eq := RustM.ok_injective h
      subst out
      simp [freeTypes]
    | Cons t rest =>
      rw [collect_free_list] at h
      obtain ⟨middle, hm, ho⟩ := bind_success _ _ _ h
      rw [collect_free_list_refines rest middle out ho v,
        collect_free_refines t acc middle hm v]
      simp [freeTypes, or_assoc]
  termination_by sizeOf ts
end

theorem free_variables_refines (t : cdsl_hm.Ty) :
    Returns (free_variables t) (fun out => ∀ v, v ∈ out.val ↔ v ∈ freeType t) := by
  intro out h v
  unfold free_variables alloc.vec.Vec.new rust_primitives.sequence.seq_empty at h
  simp only [bind_ok] at h
  simpa using collect_free_refines t (Slice.new _) out h v

mutual
  theorem free_type_decodes (t : cdsl_hm.Ty) (v : VarId) :
      v.val ∈ (decodeType t).free ↔ v ∈ freeType t := by
    cases t with
    | Var w =>
      simp only [decodeType, CDSL.HM.Ty.free, freeType, List.mem_singleton]
      exact ⟨UScalar.eq_of_val_eq, fun h => congrArg UScalar.val h⟩
    | Rigid w => simp [decodeType, CDSL.HM.Ty.free, freeType]
    | Con c args =>
      simpa [decodeType, CDSL.HM.Ty.free, freeType] using free_types_decode args v
    | Fun args result =>
      simp [decodeType, CDSL.HM.Ty.free, freeType,
        free_types_decode args v, free_type_decodes result v]
  termination_by sizeOf t

  theorem free_types_decode (ts : TyList) (v : VarId) :
      v.val ∈ (decodeTypes ts).free ↔ v ∈ freeTypes ts := by
    cases ts with
    | Nil => simp [decodeTypes, CDSL.HM.Ty.free, freeTypes]
    | Cons t rest =>
      simp [decodeTypes, CDSL.HM.Ty.free, freeTypes,
        free_type_decodes t v, free_types_decode rest v]
  termination_by sizeOf ts
end

end CDSL.HM.Refinement
