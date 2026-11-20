import Hm.Verification.Success

open CoreModels Aeneas
open Aeneas.Std hiding namespace core alloc
open RustM cdsl_hm

set_option backward.isDefEq.respectTransparency false

namespace CDSL.HM.Refinement

theorem push_success {α : Type} (xs : alloc.vec.Vec α) (x : α) :
    Returns (alloc.vec.Vec.push xs x) (fun result => result.val = xs.val ++ [x]) := by
  intro result h
  unfold alloc.vec.Vec.push at h
  obtain ⟨value, hv, he⟩ := bind_success _ _ _ h
  have eq := RustM.ok_injective he
  subst result
  unfold rust_primitives.sequence.seq_push at hv
  dsimp only at hv
  split at hv
  · have eq := RustM.ok_injective hv
    subst value
    simp
  · exact (fail_not_ok hv).elim

def mapBindings (s : cdsl_hm.Substitution) (xs : List (VarId × cdsl_hm.Ty)) :=
  xs.map (fun (v,t) => (v, substitute (lookup s.bindings.val) t))

theorem compose_first (xs : core.slice.iter.Iter (VarId × cdsl_hm.Ty))
    (s acc : cdsl_hm.Substitution) :
    Returns (cdsl_hm.Substitution.compose_loop0 xs s acc)
      (fun out => out.bindings.val = acc.bindings.val ++ mapBindings s xs.val) := by
  intro out h
  unfold Substitution.compose_loop0 at h
  rw [loop.eq_def] at h
  dsimp only at h
  cases hx : xs.val with
  | nil =>
    have body : Substitution.compose_loop0.body s xs acc = ok (.done acc) := by
      simp [Substitution.compose_loop0.body, next_nil xs hx, bind_ok]
    rw [body, bind_ok] at h
    have eq := RustM.ok_injective h
    subst out
    simp [mapBindings]
  | cons head rest =>
    obtain ⟨tail, hn, ht⟩ := next_cons xs head rest hx
    rcases head with ⟨v,t⟩
    have body : Substitution.compose_loop0.body s xs acc = (do
        let result ← alloc.vec.Vec.push acc.bindings (v, substitute (lookup s.bindings.val) t)
        ok (.cont (tail, { bindings := result }))) := by
      simp [Substitution.compose_loop0.body, hn, bind_ok, apply_refines]
    rw [body] at h
    obtain ⟨control, hc, hh⟩ := bind_success _ _ _ h
    obtain ⟨result, hp, he⟩ := bind_success _ _ _ hc
    have eq := RustM.ok_injective he
    subst control
    dsimp only at hh
    change Substitution.compose_loop0 tail s {bindings := result} = ok out at hh
    have ih := compose_first tail s {bindings := result} out hh
    have pushed := push_success acc.bindings (v, substitute (lookup s.bindings.val) t) result hp
    simp [ht, pushed, mapBindings, List.append_assoc] at ih ⊢
    exact ih
termination_by xs.val.length
decreasing_by
  all_goals simp only [ht, hx, List.length_cons]
  all_goals omega

def hasBinding (xs : List (VarId × cdsl_hm.Ty)) (v : VarId) : Bool :=
  xs.any (fun p => decide (p.1 = v))

theorem has_binding_refines (xs : core.slice.iter.Iter (VarId × cdsl_hm.Ty))
    (v : VarId) (present : Bool) :
    Substitution.compose_loop1_loop0 xs v present = ok (present || hasBinding xs.val v) := by
  unfold Substitution.compose_loop1_loop0
  rw [loop.eq_def]
  dsimp only
  cases hx : xs.val with
  | nil =>
    have body : Substitution.compose_loop1_loop0.body v xs present = ok (.done present) := by
      simp [Substitution.compose_loop1_loop0.body, next_nil xs hx, bind_ok]
    rw [body, bind_ok]
    simp [hasBinding]
  | cons head rest =>
    obtain ⟨tail, hn, ht⟩ := next_cons xs head rest hx
    rcases head with ⟨w,t⟩
    have body : Substitution.compose_loop1_loop0.body v xs present =
        ok (.cont (tail, present || decide (w = v))) := by
      simp [Substitution.compose_loop1_loop0.body, hn, bind_ok,
        VarId.Insts.CoreCmpPartialEqVarId.eq]
      split <;> simp_all
    rw [body, bind_ok]
    change Substitution.compose_loop1_loop0 tail v (present || decide (w = v)) = _
    rw [has_binding_refines tail v (present || decide (w = v))]
    simp [ht, hasBinding, Bool.or_assoc]
termination_by xs.val.length
decreasing_by
  all_goals simp only [ht, hx, List.length_cons]
  all_goals omega

def filterBindings (old xs : List (VarId × cdsl_hm.Ty)) :=
  xs.filter (fun p => !hasBinding old p.1)

theorem compose_second (xs : core.slice.iter.Iter (VarId × cdsl_hm.Ty))
    (old : alloc.vec.Vec (VarId × cdsl_hm.Ty)) (acc : cdsl_hm.Substitution) :
    Returns (Substitution.compose_loop1 xs old acc)
      (fun out => out.bindings.val = acc.bindings.val ++ filterBindings old.val xs.val) := by
  intro out h
  unfold Substitution.compose_loop1 at h
  rw [loop.eq_def] at h
  dsimp only at h
  cases hx : xs.val with
  | nil =>
    have body : Substitution.compose_loop1.body old xs acc = ok (.done acc) := by
      simp [Substitution.compose_loop1.body, next_nil xs hx, bind_ok]
    rw [body, bind_ok] at h
    have eq := RustM.ok_injective h
    subst out
    simp [filterBindings]
  | cons head rest =>
    obtain ⟨tail, hn, ht⟩ := next_cons xs head rest hx
    rcases head with ⟨v,t⟩
    have body : Substitution.compose_loop1.body old xs acc =
        (if hasBinding old.val v then ok (ControlFlow.cont (tail, acc)) else do
          let result ← alloc.vec.Vec.push acc.bindings (v,t)
          ok (ControlFlow.cont (tail, {bindings := result}))) := by
      simp [Substitution.compose_loop1.body, hn, into_iter_refines,
        has_binding_refines, bind_ok, Ty.Insts.CoreCloneClone.clone, clone_type_refines]
    rw [body] at h
    cases present : hasBinding old.val v with
    | true =>
      simp only [present, ↓reduceIte, bind_ok] at h
      change Substitution.compose_loop1 tail old acc = ok out at h
      have ih := compose_second tail old acc out h
      simpa [filterBindings, hx, ht, present] using ih
    | false =>
      simp only [present, Bool.false_eq_true, ↓reduceIte] at h
      obtain ⟨control, hc, hh⟩ := bind_success _ _ _ h
      obtain ⟨result, hp, he⟩ := bind_success _ _ _ hc
      have eq := RustM.ok_injective he
      subst control
      dsimp only at hh
      change Substitution.compose_loop1 tail old {bindings := result} = ok out at hh
      have ih := compose_second tail old {bindings := result} out hh
      have pushed := push_success acc.bindings (v,t) result hp
      simpa [filterBindings, hx, ht, present, pushed, List.append_assoc] using ih
termination_by xs.val.length
decreasing_by
  all_goals simp only [ht, hx, List.length_cons]
  all_goals omega

theorem compose_bindings (newer older : cdsl_hm.Substitution) :
    Returns (newer.compose older) (fun out => out.bindings.val =
      mapBindings newer older.bindings.val ++ filterBindings older.bindings.val newer.bindings.val) := by
  intro out h
  unfold Substitution.compose at h
  simp only [Substitution.Insts.CoreDefaultDefault.default,
    alloc.vec.Vec.Insts.CoreDefaultDefault.default, alloc.vec.Vec.new,
    rust_primitives.sequence.seq_empty, bind_ok] at h
  rw [into_iter_refines, bind_ok] at h
  obtain ⟨first, hf, hs⟩ := bind_success _ _ _ h
  have firstResult := compose_first older.bindings newer {bindings := Slice.new _} first hf
  rw [into_iter_refines, bind_ok] at hs
  have secondResult := compose_second newer.bindings older.bindings first out hs
  simpa [firstResult] using secondResult

theorem lookup_append (xs ys : List (VarId × cdsl_hm.Ty)) (v : VarId) :
    lookup (xs ++ ys) v = if hasBinding xs v then lookup xs v else lookup ys v := by
  induction xs with
  | nil => simp [hasBinding]
  | cons head rest ih =>
    rcases head with ⟨w,t⟩
    by_cases eq : w = v
    · simp [lookup, hasBinding, eq]
    · have hb : decide (w = v) = false := decide_eq_false_iff_not.mpr eq
      simp [lookup, hasBinding, eq, ih]
      split_ifs <;> simp_all

theorem lookup_filter (old xs : List (VarId × cdsl_hm.Ty)) (v : VarId) :
    lookup (filterBindings old xs) v = if hasBinding old v then .Var v else lookup xs v := by
  induction xs with
  | nil => simp [lookup, filterBindings]
  | cons head rest ih =>
    rcases head with ⟨w,t⟩
    by_cases eq : w = v
    · subst w
      cases present : hasBinding old v <;> simp [filterBindings, lookup, present] at *
      exact ih
    · cases present : hasBinding old w <;> simp [filterBindings, lookup, present, eq] at *
      all_goals exact ih

theorem has_map (s : cdsl_hm.Substitution) (xs : List (VarId × cdsl_hm.Ty)) (v : VarId) :
    hasBinding (mapBindings s xs) v = hasBinding xs v := by
  simp only [hasBinding, mapBindings, List.any_map]
  congr 1

theorem has_cons (w : VarId) (t : cdsl_hm.Ty) (xs : List (VarId × cdsl_hm.Ty)) (v : VarId) :
    hasBinding ((w,t) :: xs) v = (decide (w = v) || hasBinding xs v) := rfl

theorem lookup_map (s : cdsl_hm.Substitution) (xs : List (VarId × cdsl_hm.Ty)) (v : VarId) :
    lookup (mapBindings s xs) v =
      if hasBinding xs v then substitute (lookup s.bindings.val) (lookup xs v) else .Var v := by
  induction xs with
  | nil => simp [mapBindings, lookup, hasBinding]
  | cons head rest ih =>
    rcases head with ⟨w,t⟩
    by_cases eq : w = v
    · subst w
      simp [mapBindings, lookup, hasBinding]
    · have hb : decide (w = v) = false := decide_eq_false_iff_not.mpr eq
      change lookup ((w, substitute (lookup s.bindings.val) t) :: mapBindings s rest) v = _
      rw [has_cons, hb]
      simp only [Bool.false_or, lookup, ite_eq_right eq]
      exact ih

theorem lookup_absent (xs : List (VarId × cdsl_hm.Ty)) (v : VarId)
    (absent : hasBinding xs v = false) : lookup xs v = .Var v := by
  induction xs with
  | nil => rfl
  | cons head rest ih =>
    rcases head with ⟨w,t⟩
    simp only [hasBinding, List.any_cons, Bool.or_eq_false_iff, decide_eq_false_iff_not] at absent
    simp [lookup, absent.1, ih absent.2]

theorem compose_refines (newer older : cdsl_hm.Substitution) :
    Returns (newer.compose older) (fun out => ∀ v,
      lookup out.bindings.val v = substitute (lookup newer.bindings.val) (lookup older.bindings.val v)) := by
  intro out h v
  rw [compose_bindings newer older out h, lookup_append, has_map]
  cases present : hasBinding older.bindings.val v with
  | true => simp [present, lookup_map]
  | false => simp [present, lookup_filter, lookup_absent _ _ present, substitute]

theorem decode_large (xs : List (VarId × cdsl_hm.Ty)) (v : Nat) (large : Usize.max < v) :
    decodeSubstitution xs v = .var v := by
  induction xs with
  | nil => rfl
  | cons head rest ih =>
    rcases head with ⟨w,t⟩
    have neq : w.val ≠ v := by scalar_tac
    simp [decodeSubstitution, neq, ih]

theorem compose_semantic_refinement (newer older : cdsl_hm.Substitution) :
    Returns (newer.compose older) (fun out => ∀ v,
      decodeSubstitution out.bindings.val v =
        (decodeSubstitution older.bindings.val v).subst (decodeSubstitution newer.bindings.val)) := by
  intro out h v
  by_cases bound : v ≤ Usize.max
  · let w : VarId := Usize.ofNatCore v (by scalar_tac)
    have val : w.val = v := by simp [w]
    have eq := congrArg decodeType (compose_refines newer older out h w)
    rw [lookup_decodes, substitution_decodes _ _ (lookup_decodes newer.bindings.val),
      lookup_decodes] at eq
    simpa [val] using eq
  · have large : Usize.max < v := by omega
    simp [decode_large _ _ large, CDSL.HM.Ty.subst]

end CDSL.HM.Refinement
