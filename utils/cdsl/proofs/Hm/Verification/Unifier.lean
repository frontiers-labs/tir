import Hm.Verification.FreeVariables
import Hm.Verification.UnifierSpecification

open CoreModels Aeneas
open Aeneas.Std hiding namespace core alloc
open RustM cdsl_hm
set_option backward.isDefEq.respectTransparency false

namespace CDSL.HM.Refinement

def Solution (a b : CDSL.HM.Ty) : core.result.Result cdsl_hm.Substitution UnifyError → Prop
  | .Ok s => MostGeneral (decodeSubstitution s.bindings.val) a b
  | .Err _ => True

theorem empty_refines : Substitution.Insts.CoreDefaultDefault.default =
    ok { bindings := Slice.new (VarId × cdsl_hm.Ty) } := by
  simp [Substitution.Insts.CoreDefaultDefault.default,
    alloc.vec.Vec.Insts.CoreDefaultDefault.default, alloc.vec.Vec.new,
    rust_primitives.sequence.seq_empty, bind_ok]

theorem into_vec_refines {α : Type} (xs : Slice α) :
    alloc.slice.Slice.into_vec xs = ok xs := by
  simp [alloc.slice.Slice.into_vec, alloc.slice.Dummy.into_vec,
    rust_primitives.sequence.seq_from_boxed_slice, alloc.vec.from_seq]

def variableBranch (id : VarId) (ty : cdsl_hm.Ty) :
    RustM (core.result.Result cdsl_hm.Substitution UnifyError) := do
  let v ← free_variables ty
  let s ← alloc.vec.Vec.Insts.CoreOpsDerefDerefSlice.deref v
  let present ← core.slice.Slice.contains VarId.Insts.CoreCmpPartialEqVarId s id
  if present then
    let t ← cdsl_hm.Ty.Insts.CoreCloneClone.clone ty
    ok (.Err (.Occurs id t))
  else
    let t ← cdsl_hm.Ty.Insts.CoreCloneClone.clone ty
    let y ← lift (Std.Array.to_slice (Array.make 1#usize [(id,t)] : Array (VarId × cdsl_hm.Ty) 1#usize))
    let ret ← alloc.slice.Slice.into_vec y
    ok (.Ok {bindings := ret})

theorem variable_branch_refines (id : VarId) (ty : cdsl_hm.Ty) :
    Returns (variableBranch id ty) (Solution (.var id.val) (decodeType ty)) := by
  intro result h
  cases result with
  | Err e => trivial
  | Ok solution =>
    unfold variableBranch at h
    obtain ⟨vars, hv, h⟩ := bind_success _ _ _ h
    rw [deref_refines, bind_ok, contains_refines, bind_ok] at h
    have hf := free_variables_refines ty vars hv id
    by_cases present : id ∈ vars.val
    · simp [present, cdsl_hm.Ty.Insts.CoreCloneClone.clone, clone_type_refines, bind_ok] at h
    · have fresh : id.val ∉ (decodeType ty).free := by
        intro mem
        exact present (hf.mpr ((free_type_decodes ty id).mp mem))
      simp only [present, decide_false, Bool.false_eq_true, ↓reduceIte,
        cdsl_hm.Ty.Insts.CoreCloneClone.clone, clone_type_refines, bind_ok,
        lift, into_vec_refines] at h
      have eq := core.result.Result.Ok.inj (RustM.ok_injective h)
      subst solution
      change MostGeneral (decodeSubstitution _) (.var id.val) (decodeType ty)
      have singleton : (Std.Array.to_slice
          (Array.make 1#usize [(id,ty)] : Array (VarId × cdsl_hm.Ty) 1#usize)).val = [(id,ty)] := by
        simp [Std.Array.to_slice, Array.make]
      rw [singleton]
      have sub : decodeSubstitution [(id,ty)] = CDSL.HM.bind id.val (decodeType ty) := by
        funext v
        by_cases equal : id.val = v
        · subst v; simp [decodeSubstitution, CDSL.HM.bind]
        · have reverse : v ≠ id.val := Ne.symm equal
          simp [decodeSubstitution, CDSL.HM.bind, equal, reverse]
      rw [sub]
      exact bind_mostGeneral id.val (decodeType ty) fresh

theorem try_ok {α ε : Type} (x : α) :
    core.result.Result.Insts.CoreOpsTry_traitTry.branch (core.result.Result.Ok (E := ε) x) =
      ok (.Continue x) := rfl

theorem try_err {α ε : Type} (e : ε) :
    core.result.Result.Insts.CoreOpsTry_traitTry.branch (core.result.Result.Err (T := α) e) =
      ok (.Break (.Err e)) := rfl

theorem residual_err {T : Type} (e : UnifyError) :
    core.result.Result.Insts.CoreOpsTry_traitFromResidualResultNeverE.from_residual
      T (core.convert.From.Blanket UnifyError) (.Err e) = ok (.Err e) := by
  simp [core.result.Result.Insts.CoreOpsTry_traitFromResidualResultNeverE.from_residual,
    core.result.Result.Insts.CoreOpsTry_traitFromResidualResultInfallibleE.from_residual,
    core.convert.From.Blanket.from, bind_ok]

theorem composed_mgu (first second out : cdsl_hm.Substitution)
    (h : second.compose first = ok out) (a b x y : CDSL.HM.Ty)
    (head : MostGeneral (decodeSubstitution first.bindings.val) a b)
    (tail : MostGeneral (decodeSubstitution second.bindings.val)
      (x.subst (decodeSubstitution first.bindings.val))
      (y.subst (decodeSubstitution first.bindings.val))) :
    MostGeneral (decodeSubstitution out.bindings.val) (.app a x) (.app b y) := by
  have eq := funext (compose_semantic_refinement second first out h)
  rw [eq]
  exact mgu_application _ _ _ _ _ _ head tail

theorem unify_refines : ∀ a b,
    Returns (cdsl_hm.unify a b) (Solution (decodeType a) (decodeType b)) := by
  refine cdsl_hm.unify.fixpoint_induct
    (motive_1 := fun uf => ∀ a b, Returns (uf a b) (Solution (decodeType a) (decodeType b)))
    (motive_2 := fun uf => ∀ a b, Returns (uf a b) (Solution (decodeTypes a) (decodeTypes b)))
    ?_ ?_ ?_ ?_
  · exact returns_binary_admissible _
  · exact returns_binary_admissible _
  · intro uf um huf hum a b result h
    cases result with
    | Err e => trivial
    | Ok solution =>
      dsimp only at h
      simp only [cdsl_hm.Ty.Insts.CoreCmpPartialEqTy.eq, equal_type_refines, bind_ok] at h
      by_cases same : a = b
      · subst b
        simp only [sameType, decide_true, ↓reduceIte, empty_refines, bind_ok] at h
        have eq := RustM.ok_injective h
        cases eq
        exact mgu_identity (decodeType a)
      · simp only [sameType, same, decide_false, Bool.false_eq_true, ↓reduceIte] at h
        cases a with
        | Var id =>
          change variableBranch id b = ok (.Ok solution) at h
          exact variable_branch_refines id b (.Ok solution) h
        | Rigid id =>
          cases b with
          | Var v =>
            change variableBranch v (.Rigid id) = ok (.Ok solution) at h
            exact mgu_symmetric _ _ _ (variable_branch_refines v (.Rigid id) (.Ok solution) h)
          | Rigid _ | Con _ _ | Fun _ _ =>
            simp [cdsl_hm.Ty.Insts.CoreCloneClone.clone, clone_type_refines, bind_ok] at h
        | Con id args =>
          cases b with
          | Var v =>
            change variableBranch v (.Con id args) = ok (.Ok solution) at h
            exact mgu_symmetric _ _ _ (variable_branch_refines v (.Con id args) (.Ok solution) h)
          | Rigid _ | Fun _ _ =>
            simp [cdsl_hm.Ty.Insts.CoreCloneClone.clone, clone_type_refines, bind_ok] at h
          | Con other rest =>
            simp only [ConId.Insts.CoreCmpPartialEqConId.eq, bind_ok] at h
            by_cases eq : id = other
            · subst other
              simp only [decide_true, ↓reduceIte] at h
              exact mgu_wrapped _ (id.val + 3) _ _ (hum args rest (.Ok solution) h)
            · simp [eq, cdsl_hm.Ty.Insts.CoreCloneClone.clone, clone_type_refines, bind_ok] at h
        | Fun args ret =>
          cases b with
          | Var v =>
            change variableBranch v (.Fun args ret) = ok (.Ok solution) at h
            exact mgu_symmetric _ _ _ (variable_branch_refines v (.Fun args ret) (.Ok solution) h)
          | Rigid _ | Con _ _ =>
            simp [cdsl_hm.Ty.Insts.CoreCloneClone.clone, clone_type_refines, bind_ok] at h
          | Fun rest output =>
            dsimp only at h
            obtain ⟨r, hr, h⟩ := bind_success _ _ _ h
            cases r with
            | Err e => simp [try_err, residual_err, bind_ok] at h
            | Ok first =>
              simp only [try_ok, bind_ok] at h
              obtain ⟨x, hx, dx⟩ := apply_semantic_refinement first ret
              obtain ⟨y, hy, dy⟩ := apply_semantic_refinement first output
              rw [hx, bind_ok, hy, bind_ok] at h
              obtain ⟨r, hr2, h⟩ := bind_success _ _ _ h
              cases r with
              | Err e => simp [try_err, residual_err, bind_ok] at h
              | Ok second =>
                simp only [try_ok, bind_ok] at h
                obtain ⟨out, hc, he⟩ := bind_success _ _ _ h
                have eq := core.result.Result.Ok.inj (RustM.ok_injective he)
                subst solution
                have head := hum args rest (.Ok first) hr
                have tail := huf x y (.Ok second) hr2
                change MostGeneral _ _ _ at head tail
                rw [dx, dy] at tail
                exact composed_mgu first second out hc _ _ _ _ (mgu_wrapped _ 0 _ _ head) tail
  · intro uf um huf hum a b result h
    cases result with
    | Err e => trivial
    | Ok solution =>
      dsimp only at h
      cases a with
      | Nil =>
        cases b with
        | Nil =>
          simp only [empty_refines, bind_ok] at h
          have eq := core.result.Result.Ok.inj (RustM.ok_injective h)
          subst solution
          exact mgu_identity _
        | Cons _ _ =>
          obtain ⟨_, _, h⟩ := bind_success _ _ _ h
          obtain ⟨_, _, h⟩ := bind_success _ _ _ h
          cases RustM.ok_injective h
      | Cons t ts =>
        cases b with
        | Nil =>
          obtain ⟨_, _, h⟩ := bind_success _ _ _ h
          obtain ⟨_, _, h⟩ := bind_success _ _ _ h
          cases RustM.ok_injective h
        | Cons u us =>
          dsimp only at h
          obtain ⟨r, hr, h⟩ := bind_success _ _ _ h
          cases r with
          | Err e => simp [try_err, residual_err, bind_ok] at h
          | Ok first =>
            simp only [try_ok, bind_ok] at h
            obtain ⟨xs, hx, dx⟩ := apply_list_semantic_refinement first ts
            obtain ⟨ys, hy, dy⟩ := apply_list_semantic_refinement first us
            rw [hx, bind_ok, hy, bind_ok] at h
            obtain ⟨r, hr2, h⟩ := bind_success _ _ _ h
            cases r with
            | Err e => simp [try_err, residual_err, bind_ok] at h
            | Ok second =>
              simp only [try_ok, bind_ok] at h
              obtain ⟨out, hc, he⟩ := bind_success _ _ _ h
              have eq := core.result.Result.Ok.inj (RustM.ok_injective he)
              subst solution
              have head := huf t u (.Ok first) hr
              have tail := hum xs ys (.Ok second) hr2
              change MostGeneral _ _ _ at head tail
              rw [dx, dy] at tail
              exact composed_mgu first second out hc _ _ _ _ (mgu_wrapped _ 2 _ _ head) tail

end CDSL.HM.Refinement
