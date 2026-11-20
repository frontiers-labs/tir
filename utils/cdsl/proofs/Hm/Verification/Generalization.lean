import Hm.Verification.Unifier

open CoreModels Aeneas
open Aeneas.Std hiding namespace core alloc
open RustM cdsl_hm
set_option backward.isDefEq.respectTransparency false

namespace CDSL.HM.Refinement

def envFree (v : VarId) (env : List cdsl_hm.Scheme) : Prop :=
  ∃ s ∈ env, v ∈ freeType s.ty ∧ v ∉ s.quantified.val

theorem free_env_loop_refines (xs : core.slice.iter.Iter cdsl_hm.Scheme) (v : VarId) :
    Returns (free_in_environment_loop xs v) (fun result => result = true ↔ envFree v xs.val) := by
  intro out h
  unfold free_in_environment_loop at h
  rw [loop.eq_def] at h
  cases hx : xs.val with
  | nil =>
    have body : free_in_environment_loop.body v xs = ok (ControlFlow.done false) := by
      simp [free_in_environment_loop.body, next_nil xs hx, bind_ok]
    rw [body, bind_ok] at h
    have eq := RustM.ok_injective h
    subst out
    simp [envFree]
  | cons scheme rest =>
    obtain ⟨tail, hn, ht⟩ := next_cons xs scheme rest hx
    have body : free_in_environment_loop.body v xs =
        (if v ∈ scheme.quantified.val then ok (ControlFlow.cont tail) else do
          let vars ← free_variables scheme.ty
          if v ∈ vars.val then ok (ControlFlow.done true) else ok (ControlFlow.cont tail)) := by
      simp [free_in_environment_loop.body, hn, bind_ok, deref_refines, contains_refines]
    rw [body] at h
    by_cases quantified : v ∈ scheme.quantified.val
    · simp only [quantified, ↓reduceIte, bind_ok] at h
      change free_in_environment_loop tail v = ok out at h
      have ih := free_env_loop_refines tail v out h
      simpa [envFree, hx, ht, quantified] using ih
    · simp only [quantified, ↓reduceIte] at h
      obtain ⟨control, hc, hh⟩ := bind_success _ _ _ h
      obtain ⟨vars, hv, he⟩ := bind_success _ _ _ hc
      have fv := free_variables_refines scheme.ty vars hv v
      by_cases present : v ∈ vars.val
      · simp only [present, ↓reduceIte] at he
        have eq := RustM.ok_injective he
        subst control
        have eq := RustM.ok_injective hh
        subst out
        have free := fv.mp present
        simp [envFree, quantified, free]
      · simp only [present, ↓reduceIte] at he
        have eq := RustM.ok_injective he
        subst control
        change free_in_environment_loop tail v = ok out at hh
        have ih := free_env_loop_refines tail v out hh
        have absent : v ∉ freeType scheme.ty := fun h => present (fv.mpr h)
        simpa [envFree, hx, ht, absent] using ih
termination_by xs.val.length
decreasing_by all_goals simp only [ht, hx, List.length_cons]; omega

theorem slice_iter_refines {α : Type} (xs : Slice α) :
    core.SharedASlice.Insts.CoreIterTraitsCollectIntoIteratorSharedATIter.into_iter xs = ok xs := by
  simp [core.SharedASlice.Insts.CoreIterTraitsCollectIntoIteratorSharedATIter.into_iter,
    core.slice.Slice.iter, rust_primitives.sequence.seq_from_slice]

theorem free_env_refines (v : VarId) (env : Slice cdsl_hm.Scheme) :
    Returns (free_in_environment v env) (fun result => result = true ↔ envFree v env.val) := by
  unfold free_in_environment
  rw [slice_iter_refines, bind_ok]
  exact free_env_loop_refines env v

theorem generalize_loop_refines (xs : core.slice.iter.Iter VarId) (env : Slice cdsl_hm.Scheme)
    (acc : alloc.vec.Vec VarId) :
    Returns (generalize_loop xs env acc) (fun out => ∀ v,
      v ∈ out.val ↔ v ∈ acc.val ∨ (v ∈ xs.val ∧ ¬envFree v env.val)) := by
  intro out h v
  unfold generalize_loop at h
  rw [loop.eq_def] at h
  dsimp only at h
  cases hx : xs.val with
  | nil =>
    have body : generalize_loop.body env xs acc = ok (ControlFlow.done acc) := by
      simp [generalize_loop.body, next_nil xs hx, bind_ok]
    rw [body, bind_ok] at h
    have eq := RustM.ok_injective h
    subst out
    simp
  | cons w rest =>
    obtain ⟨tail, hn, ht⟩ := next_cons xs w rest hx
    have body : generalize_loop.body env xs acc = (do
        let present ← free_in_environment w env
        if present then ok (ControlFlow.cont (tail,acc)) else do
          let result ← alloc.vec.Vec.push acc w
          ok (ControlFlow.cont (tail,result))) := by
      simp [generalize_loop.body, hn, bind_ok]
    rw [body] at h
    obtain ⟨control, hc, hh⟩ := bind_success _ _ _ h
    obtain ⟨present, hp, he⟩ := bind_success _ _ _ hc
    have hequiv := free_env_refines w env present hp
    cases present with
    | true =>
      simp only [↓reduceIte] at he
      have eq := RustM.ok_injective he
      subst control
      change generalize_loop tail env acc = ok out at hh
      have ih := generalize_loop_refines tail env acc out hh v
      have free : envFree w env.val := hequiv.mp rfl
      simp only [List.mem_cons, ht] at ih ⊢
      rw [ih]
      by_cases equal : v = w
      · subst v; simp_all
      · simp [equal]
    | false =>
      simp only [Bool.false_eq_true, ↓reduceIte] at he
      obtain ⟨pushed, hpush, hcontrol⟩ := bind_success _ _ _ he
      have eq := RustM.ok_injective hcontrol
      subst control
      change generalize_loop tail env pushed = ok out at hh
      have ih := generalize_loop_refines tail env pushed out hh v
      have added := push_success acc w pushed hpush
      have nonfree : ¬envFree w env.val := fun h => Bool.false_ne_true (hequiv.mpr h)
      simp only [added, ht, List.mem_append, List.mem_cons] at ih ⊢
      rw [ih]
      by_cases equal : v = w
      · subst v; simp_all
      · simp [equal]
termination_by xs.val.length
decreasing_by all_goals simp only [ht, hx, List.length_cons]; omega

/-- Every generalizable variable is quantified, and environment-free variables are excluded. -/
theorem generalize_refines (env : Slice cdsl_hm.Scheme) (ty : cdsl_hm.Ty) :
    Returns (generalize env ty) (fun out => out.ty = ty ∧ ∀ v,
      v ∈ out.quantified.val ↔ v ∈ freeType ty ∧ ¬envFree v env.val) := by
  intro out h
  unfold generalize alloc.vec.Vec.new rust_primitives.sequence.seq_empty at h
  simp only [bind_ok] at h
  obtain ⟨vars, hv, h⟩ := bind_success _ _ _ h
  rw [into_iter_refines, bind_ok] at h
  obtain ⟨quantified, hq, h⟩ := bind_success _ _ _ h
  rw [cdsl_hm.Ty.Insts.CoreCloneClone.clone, clone_type_refines, bind_ok] at h
  have eq := RustM.ok_injective h
  subst out
  refine ⟨rfl, ?_⟩
  intro v
  have result := generalize_loop_refines vars env (Slice.new _) quantified hq v
  have fv := free_variables_refines ty vars hv v
  simpa [fv] using result

def decodeScheme (s : cdsl_hm.Scheme) : CDSL.HM.Scheme :=
  {quantified := s.quantified.val.map UScalar.val, body := decodeType s.ty}

theorem quantified_decodes (s : cdsl_hm.Scheme) (v : VarId) :
    v.val ∈ (decodeScheme s).quantified ↔ v ∈ s.quantified.val := by
  simp only [decodeScheme, List.mem_map]
  constructor
  · rintro ⟨w, hw, eq⟩
    have equal := UScalar.eq_of_val_eq eq
    subst w
    exact hw
  · intro h
    exact ⟨v,h,rfl⟩

theorem scheme_free_decodes (s : cdsl_hm.Scheme) (v : VarId) :
    v.val ∈ (decodeScheme s).free ↔ v ∈ freeType s.ty ∧ v ∉ s.quantified.val := by
  have body : (decodeScheme s).body = decodeType s.ty := rfl
  simp [CDSL.HM.Scheme.free, body, free_type_decodes, quantified_decodes]

theorem env_free_decodes (env : List cdsl_hm.Scheme) (v : VarId) :
    v.val ∈ CDSL.HM.Env.free (env.map decodeScheme) ↔ envFree v env := by
  simp [CDSL.HM.Env.free, envFree, scheme_free_decodes]

/-- The production generalizer obeys the generalization premise of the HM let rule. -/
theorem generalize_semantic_refinement (env : Slice cdsl_hm.Scheme) (ty : cdsl_hm.Ty) :
    Returns (generalize env ty) (fun out =>
      Generalizes (env.val.map decodeScheme) (decodeType ty) (decodeScheme out)) := by
  intro out h
  obtain ⟨body, quantified⟩ := generalize_refines env ty out h
  refine ⟨congrArg decodeType body, ?_⟩
  intro n hn
  obtain ⟨v,hv,equal⟩ := List.mem_map.mp hn
  subst n
  have parts := (quantified v).mp hv
  exact ⟨(free_type_decodes ty v).mpr parts.1,
    fun h => parts.2 ((env_free_decodes env.val v).mp h)⟩

end CDSL.HM.Refinement
