import Hm.Verification.Instantiation

open CoreModels Aeneas
open Aeneas.Std hiding namespace core alloc
open RustM cdsl_hm
set_option backward.isDefEq.respectTransparency false

namespace CDSL.HM.Refinement

theorem clone_vec_loop_refines {α : Type} (inst : core.clone.Clone α)
    (identity : ∀ x, inst.clone x = ok x) (xs : core.slice.iter.Iter α)
    (acc : rust_primitives.sequence.Seq α) :
    Returns (alloc.vec.Vec.Insts.CoreCloneClone.clone_loop inst xs acc)
      (fun out => out.val = acc.val ++ xs.val) := by
  intro out h
  unfold alloc.vec.Vec.Insts.CoreCloneClone.clone_loop at h
  rw [loop.eq_def] at h
  dsimp only at h
  cases hx : xs.val with
  | nil =>
    have body : alloc.vec.Vec.Insts.CoreCloneClone.clone_loop.body inst xs acc = ok (.done acc) := by
      simp [alloc.vec.Vec.Insts.CoreCloneClone.clone_loop.body, next_nil xs hx, bind_ok]
    rw [body, bind_ok] at h
    have eq := RustM.ok_injective h
    subst out
    simp
  | cons x rest =>
    obtain ⟨tail, hn, ht⟩ := next_cons xs x rest hx
    have body : alloc.vec.Vec.Insts.CoreCloneClone.clone_loop.body inst xs acc = (do
        let pushed ← rust_primitives.sequence.seq_push acc x
        ok (ControlFlow.cont (tail,pushed))) := by
      simp [alloc.vec.Vec.Insts.CoreCloneClone.clone_loop.body, hn, identity, bind_ok]
    rw [body] at h
    obtain ⟨control,hcontrol,hloop⟩ := bind_success _ _ _ h
    obtain ⟨pushed,hpush,he⟩ := bind_success _ _ _ hcontrol
    have eq := RustM.ok_injective he
    subst control
    change alloc.vec.Vec.Insts.CoreCloneClone.clone_loop inst tail pushed = ok out at hloop
    have ih := clone_vec_loop_refines inst identity tail pushed out hloop
    have hp : alloc.vec.Vec.push acc x = ok pushed := by simp [alloc.vec.Vec.push,hpush]
    have added := push_success acc x pushed hp
    simpa [added,ht,List.append_assoc] using ih
termination_by xs.val.length
decreasing_by all_goals simp only [ht, hx, List.length_cons]; omega

theorem clone_vars_refines (xs : alloc.vec.Vec VarId) :
    Returns (alloc.vec.Vec.Insts.CoreCloneClone.clone VarId.Insts.CoreCloneClone xs)
      (fun out => out.val = xs.val) := by
  intro out h
  unfold alloc.vec.Vec.Insts.CoreCloneClone.clone at h
  simp only [rust_primitives.sequence.seq_empty, bind_ok, deref_refines] at h
  have it : core.slice.Slice.iter xs = ok xs := by
    simp [core.slice.Slice.iter,rust_primitives.sequence.seq_from_slice]
  rw [it,bind_ok] at h
  obtain ⟨result,hr,he⟩ := bind_success _ _ _ h
  have eq := RustM.ok_injective he
  subst out
  simpa using clone_vec_loop_refines VarId.Insts.CoreCloneClone (fun _ => rfl) xs (Slice.new _) result hr

def unboundBindings (q : List VarId) (xs : List (VarId × cdsl_hm.Ty)) :=
  xs.filter (fun p => !(q.contains p.1))

theorem scheme_loop_refines (xs : core.slice.iter.Iter (VarId × cdsl_hm.Ty))
    (scheme : cdsl_hm.Scheme) (acc : cdsl_hm.Substitution) :
    Returns (Substitution.apply_scheme_loop xs scheme acc) (fun out => out.1 = scheme ∧
      out.2.bindings.val = acc.bindings.val ++ unboundBindings scheme.quantified.val xs.val) := by
  intro out h
  unfold Substitution.apply_scheme_loop at h
  rw [loop.eq_def] at h
  dsimp only at h
  cases hx : xs.val with
  | nil =>
    have body : Substitution.apply_scheme_loop.body scheme xs acc = ok (.done (scheme,acc)) := by
      simp [Substitution.apply_scheme_loop.body,next_nil xs hx,bind_ok]
    rw [body,bind_ok] at h
    have eq := RustM.ok_injective h
    subst out
    simp [unboundBindings]
  | cons pair rest =>
    obtain ⟨tail,hn,ht⟩ := next_cons xs pair rest hx
    rcases pair with ⟨v,t⟩
    have body : Substitution.apply_scheme_loop.body scheme xs acc =
        (if v ∈ scheme.quantified.val then ok (ControlFlow.cont (tail,acc)) else do
          let pushed ← alloc.vec.Vec.push acc.bindings (v,t)
          ok (ControlFlow.cont (tail,{bindings := pushed}))) := by
      simp [Substitution.apply_scheme_loop.body,hn,bind_ok,deref_refines,contains_refines,
        cdsl_hm.Ty.Insts.CoreCloneClone.clone,clone_type_refines]
    rw [body] at h
    by_cases bound : v ∈ scheme.quantified.val
    · simp only [bound,↓reduceIte,bind_ok] at h
      change Substitution.apply_scheme_loop tail scheme acc = ok out at h
      have ih := scheme_loop_refines tail scheme acc out h
      simpa [ht,unboundBindings,bound] using ih
    · simp only [bound,↓reduceIte] at h
      obtain ⟨control,hcontrol,hloop⟩ := bind_success _ _ _ h
      obtain ⟨pushed,hpush,he⟩ := bind_success _ _ _ hcontrol
      have eq := RustM.ok_injective he
      subst control
      change Substitution.apply_scheme_loop tail scheme {bindings := pushed} = ok out at hloop
      have ih := scheme_loop_refines tail scheme {bindings := pushed} out hloop
      have added := push_success acc.bindings (v,t) pushed hpush
      simpa [ht,unboundBindings,bound,added,List.append_assoc] using ih
termination_by xs.val.length
decreasing_by all_goals simp only [ht,hx,List.length_cons]; omega

theorem unbound_lookup (q : List VarId) (xs : List (VarId × cdsl_hm.Ty)) (v : Nat) :
    decodeSubstitution (unboundBindings q xs) v =
      if v ∈ q.map UScalar.val then CDSL.HM.Ty.var v else decodeSubstitution xs v := by
  induction xs with
  | nil => simp [unboundBindings,decodeSubstitution]
  | cons pair rest ih =>
    rcases pair with ⟨w,t⟩
    have bound : w ∈ q ↔ w.val ∈ q.map UScalar.val := by
      constructor
      · intro h; exact List.mem_map.mpr ⟨w,h,rfl⟩
      · rintro h
        obtain ⟨u,hu,eq⟩ := List.mem_map.mp h
        have equal := UScalar.eq_of_val_eq eq
        subst u
        exact hu
    by_cases eq : w.val = v
    · subst v
      by_cases member : w ∈ q
      · simp [unboundBindings,member,bound.mp member] at *
        exact ih
      · have absent : w.val ∉ q.map UScalar.val := fun h => member (bound.mpr h)
        simp [unboundBindings,decodeSubstitution,member,absent]
    · by_cases member : w ∈ q <;>
        simpa [unboundBindings,decodeSubstitution,member,eq] using ih

/-- Scheme substitution leaves quantified IDs unchanged; callers must keep replacement ranges fresh. -/
theorem apply_scheme_refines (s : cdsl_hm.Substitution) (scheme : cdsl_hm.Scheme) :
    Returns (s.apply_scheme scheme) (fun out => out.quantified.val = scheme.quantified.val ∧
      (decodeScheme out).body = (decodeScheme scheme).body.subst
        (fun v => if v ∈ (decodeScheme scheme).quantified then .var v else decodeSubstitution s.bindings.val v)) := by
  intro out h
  unfold Substitution.apply_scheme at h
  rw [empty_refines,bind_ok,into_iter_refines,bind_ok] at h
  obtain ⟨pair,hpair,h⟩ := bind_success _ _ _ h
  rcases pair with ⟨scheme1,free⟩
  obtain ⟨same,bindings⟩ := scheme_loop_refines s.bindings scheme {bindings := Slice.new _} (scheme1,free) hpair
  dsimp only at same bindings
  subst scheme1
  obtain ⟨q,hq,h⟩ := bind_success _ _ _ h
  have quantified := clone_vars_refines scheme.quantified q hq
  obtain ⟨ty,ht,h⟩ := bind_success _ _ _ h
  have eq := RustM.ok_injective h
  subst out
  obtain ⟨ty1,ht1,semantic⟩ := apply_semantic_refinement free scheme.ty
  rw [ht] at ht1
  have sameType := RustM.ok_injective ht1
  subst ty1
  refine ⟨quantified,?_⟩
  change decodeType ty = _
  rw [semantic]
  simp only [Slice.new_val,List.nil_append] at bindings
  rw [bindings]
  congr 1
  funext v
  exact unbound_lookup scheme.quantified.val s.bindings.val v

end CDSL.HM.Refinement
