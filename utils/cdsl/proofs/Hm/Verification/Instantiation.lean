import Hm.Verification.Generalization

open CoreModels Aeneas
open Aeneas.Std hiding namespace core alloc
open RustM cdsl_hm
set_option backward.isDefEq.respectTransparency false

namespace CDSL.HM.Refinement

theorem instantiate_loop_refines
    (xs : core.slice.iter.Iter VarId) (ty : cdsl_hm.Ty) (start : Usize)
    (s : cdsl_hm.Substitution) (out : cdsl_hm.Ty) (finish : Usize)
    (h : instantiate_loop xs ty start s = ok (.Ok out, finish)) :
    ∃ final : cdsl_hm.Substitution, ∃ added : List (VarId × cdsl_hm.Ty),
      final.bindings.val = s.bindings.val ++ added ∧
      added.map Prod.fst = xs.val ∧
      (∀ pair ∈ added, ∃ id, pair.2 = cdsl_hm.Ty.Var id ∧ start.val ≤ id.val ∧ id.val < finish.val) ∧
      decodeType out = (decodeType ty).subst (decodeSubstitution final.bindings.val) ∧
      finish.val = start.val + xs.val.length := by
  unfold instantiate_loop at h
  rw [loop.eq_def] at h
  dsimp only at h
  cases hx : xs.val with
  | nil =>
    obtain ⟨result, hr, semantic⟩ := apply_semantic_refinement s ty
    have body : instantiate_loop.body ty xs start s = ok (.done (.Ok result,start)) := by
      simp [instantiate_loop.body, next_nil xs hx, bind_ok, hr]
    rw [body, bind_ok] at h
    have eq := RustM.ok_injective h
    have values := Prod.mk.inj eq
    have types := core.result.Result.Ok.inj values.1
    subst out
    have finishes := values.2
    subst finish
    exact ⟨s, [], by simp, by simp, by simp, semantic, by simp⟩
  | cons v rest =>
    obtain ⟨tail, hn, ht⟩ := next_cons xs v rest hx
    obtain ⟨freshResult, next, hf, prop⟩ := fresh_refines start
    cases freshResult with
    | Err e =>
      have body : instantiate_loop.body ty xs start s = ok (.done (.Err e,next)) := by
        simp [instantiate_loop.body, hn, hf, try_err, residual_err, bind_ok]
      rw [body, bind_ok] at h
      cases RustM.ok_injective h
    | Ok value =>
      cases value with
      | Rigid _ | Con _ _ | Fun _ _ => exact prop.elim
      | Var id =>
        obtain ⟨same, advanced⟩ := prop
        subst id
        have body : instantiate_loop.body ty xs start s = (do
            let bindings ← alloc.vec.Vec.push s.bindings (v, cdsl_hm.Ty.Var start)
            ok (ControlFlow.cont (tail,next,{bindings := bindings}))) := by
          simp [instantiate_loop.body, hn, hf, try_ok, bind_ok]
        rw [body] at h
        obtain ⟨control,hcontrol,hloop⟩ := bind_success _ _ _ h
        obtain ⟨pushed,hpush,he⟩ := bind_success _ _ _ hcontrol
        have eq := RustM.ok_injective he
        subst control
        dsimp only at hloop
        change instantiate_loop tail ty next {bindings := pushed} = ok (.Ok out,finish) at hloop
        obtain ⟨final,added,hbindings,hkeys,hrange,hsemantic,hcount⟩ :=
          instantiate_loop_refines tail ty next {bindings := pushed} out finish hloop
        have pushedKeys := push_success s.bindings (v, cdsl_hm.Ty.Var start) pushed hpush
        refine ⟨final, (v,cdsl_hm.Ty.Var start)::added, ?_, ?_, ?_, hsemantic, ?_⟩
        · simpa [pushedKeys, List.append_assoc] using hbindings
        · simp [hkeys, ht]
        · intro pair member
          rcases List.mem_cons.mp member with equal | member
          · subst pair
            refine ⟨start,rfl,by omega,?_⟩
            omega
          · obtain ⟨id,htype,lo,hi⟩ := hrange pair member
            exact ⟨id,htype,by omega,hi⟩
        · simp only [ht] at hcount
          simp only [List.length_cons]
          omega
termination_by xs.val.length
decreasing_by all_goals simp only [ht, hx, List.length_cons]; omega

theorem decode_outside (bindings : List (VarId × cdsl_hm.Ty)) (v : Nat)
    (outside : ∀ pair ∈ bindings, pair.1.val ≠ v) :
    decodeSubstitution bindings v = .var v := by
  induction bindings with
  | nil => rfl
  | cons pair rest ih =>
    have head := outside pair (by simp)
    have tail : ∀ pair ∈ rest, pair.1.val ≠ v := fun p hp => outside p (by simp [hp])
    simp [decodeSubstitution, head, ih tail]

/-- Successful instantiation is an instance of the quantified scheme, with fresh IDs in the allocated interval. -/
theorem instantiate_refines (scheme : cdsl_hm.Scheme) (start : Usize)
    (out : cdsl_hm.Ty) (finish : Usize)
    (h : instantiate scheme start = ok (.Ok out, finish)) :
    Instance (decodeScheme scheme) (decodeType out) ∧
      finish.val = start.val + scheme.quantified.val.length ∧
      ∃ substitution : CDSL.HM.Substitution,
        (decodeType scheme.ty).subst substitution = decodeType out ∧
        (∀ v ∈ (decodeScheme scheme).quantified,
          ∃ fresh, substitution v = CDSL.HM.Ty.var fresh ∧ start.val ≤ fresh ∧ fresh < finish.val) := by
  unfold instantiate at h
  rw [empty_refines, bind_ok, into_iter_refines, bind_ok] at h
  obtain ⟨final,added,bindings,keys,range,semantic,count⟩ :=
    instantiate_loop_refines scheme.quantified scheme.ty start {bindings := Slice.new _} out finish h
  simp only [Slice.new_val, List.nil_append] at bindings
  have outside : ∀ v, v ∉ (decodeScheme scheme).quantified →
      decodeSubstitution final.bindings.val v = .var v := by
    intro v absent
    apply decode_outside
    intro pair member equal
    rw [bindings] at member
    have key : pair.1 ∈ scheme.quantified.val := by rw [←keys]; exact List.mem_map.mpr ⟨pair,member,rfl⟩
    apply absent
    exact List.mem_map.mpr ⟨pair.1,key,equal⟩
  refine ⟨⟨decodeSubstitution final.bindings.val,outside,semantic.symm⟩,count,?_⟩
  refine ⟨decodeSubstitution final.bindings.val,semantic.symm,?_⟩
  intro v member
  rw [bindings]
  have support : v ∈ added.map (fun pair => pair.1.val) := by
    have maps := congrArg (List.map UScalar.val) keys
    have mem : v ∈ scheme.quantified.val.map UScalar.val := member
    rw [←maps] at mem
    simpa [List.map_map] using mem
  clear outside semantic bindings keys h final
  induction added with
  | nil => simp at support
  | cons pair rest ih =>
    rcases pair with ⟨key,value⟩
    by_cases eq : key.val = v
    · obtain ⟨id,valueEq,lo,hi⟩ := range (key,value) (by simp)
      change value = cdsl_hm.Ty.Var id at valueEq
      subst value
      refine ⟨id.val, ?_,lo,hi⟩
      simp [decodeSubstitution, eq, decodeType]
    · have tailSupport : v ∈ rest.map (fun pair => pair.1.val) := by simpa [eq, Ne.symm eq] using support
      have tailRange := fun pair (hp : pair ∈ rest) => range pair (by simp [hp])
      simpa [decodeSubstitution, eq] using ih tailRange tailSupport

end CDSL.HM.Refinement
