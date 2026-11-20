import Hm.Extraction
import Hm.Verification.Specification

open CoreModels Aeneas
open Aeneas.Std hiding namespace core alloc
open RustM

namespace CDSL.HM.Refinement

/-- The allocator must return its old identity and advance without wrapping. -/
def FreshResult (old : Usize) (result : core.result.Result cdsl_hm.Ty cdsl_hm.UnifyError)
    (next : Usize) : Prop :=
  match result with
  | .Ok (.Var id) => id = old ∧ next.val = old.val + 1
  | .Err .FreshExhausted => old.val = Usize.max ∧ next = old
  | _ => False

theorem fresh_refines (old : Usize) :
    ∃ result next, cdsl_hm.fresh old = ok (result, next) ∧ FreshResult old result next := by
  have spec := UScalar.overflowing_add_eq old (1#usize)
  rcases pair : UScalar.overflowing_add old (1#usize) with ⟨new, flag⟩
  simp only [pair] at spec
  simp at spec
  unfold cdsl_hm.fresh core.num.Usize.checked_add core.num.Usize.overflowing_add
    rust_primitives.arithmetic.overflowing_add_usize
  simp only [bind_ok, pair]
  split at spec
  · obtain ⟨_, overflow⟩ := spec
    refine ⟨.Err .FreshExhausted, old, ?_, ?_⟩
    · simp [overflow, core.option.Option.ok_or,
        core.result.Result.Insts.CoreOpsTry_traitTry.branch,
        core.result.Result.Insts.CoreOpsTry_traitTryTResultInfallibleE.branch,
        core.result.Result.Insts.CoreOpsTry_traitFromResidualResultNeverE.from_residual,
        core.result.Result.Insts.CoreOpsTry_traitFromResidualResultInfallibleE.from_residual,
        core.convert.From.Blanket.from, bind_ok]
    · simp only [FreshResult, and_true]
      scalar_tac
  · obtain ⟨value, overflow⟩ := spec
    refine ⟨.Ok (.Var old), new, ?_, ?_⟩
    · simp [overflow, core.option.Option.ok_or,
        core.result.Result.Insts.CoreOpsTry_traitTry.branch,
        core.result.Result.Insts.CoreOpsTry_traitTryTResultInfallibleE.branch, bind_ok]
    · exact ⟨rfl, value⟩

mutual
  theorem clone_type_refines (t : cdsl_hm.Ty) : cdsl_hm.clone_type t = ok t := by
    cases t with
    | Var v => rw [cdsl_hm.clone_type]
    | Rigid v => rw [cdsl_hm.clone_type]
    | Con c args => simp [cdsl_hm.clone_type, clone_types_refines args, bind_ok]
    | Fun args result =>
      simp [cdsl_hm.clone_type, clone_types_refines args, clone_type_refines result, bind_ok]
  termination_by sizeOf t

  theorem clone_types_refines (ts : cdsl_hm.TyList) : cdsl_hm.clone_types ts = ok ts := by
    cases ts with
    | Nil => rw [cdsl_hm.clone_types]
    | Cons t rest =>
      simp [cdsl_hm.clone_types, clone_type_refines t, clone_types_refines rest, bind_ok]
  termination_by sizeOf ts
end

noncomputable def sameType (a b : cdsl_hm.Ty) : Bool :=
  @decide (a = b) (Classical.propDecidable _)
noncomputable def sameTypes (a b : cdsl_hm.TyList) : Bool :=
  @decide (a = b) (Classical.propDecidable _)

mutual
  theorem equal_type_refines (a b : cdsl_hm.Ty) :
      cdsl_hm.equal_type a b = ok (sameType a b) := by
    cases a <;> cases b <;> rw [cdsl_hm.equal_type]
    all_goals try simp [sameType, cdsl_hm.VarId.Insts.CoreCmpPartialEqVarId.eq,
      cdsl_hm.RigidId.Insts.CoreCmpPartialEqRigidId.eq]
    case Con.Con x xs y ys =>
      simp [cdsl_hm.ConId.Insts.CoreCmpPartialEqConId.eq, bind_ok,
        equal_types_refines xs ys, sameTypes, cdsl_hm.Ty.Con.injEq]
      split <;> simp_all
    case Fun.Fun xs x ys y =>
      simp [equal_types_refines xs ys, equal_type_refines x y,
        bind_ok, sameType, sameTypes, cdsl_hm.Ty.Fun.injEq]
      split <;> simp_all
  termination_by sizeOf a

  theorem equal_types_refines (a b : cdsl_hm.TyList) :
      cdsl_hm.equal_types a b = ok (sameTypes a b) := by
    cases a <;> cases b <;> rw [cdsl_hm.equal_types]
    all_goals try simp [sameTypes]
    case Cons.Cons x xs y ys =>
      simp [equal_type_refines x y, equal_types_refines xs ys,
        bind_ok, sameType, sameTypes, cdsl_hm.TyList.Cons.injEq]
      split <;> simp_all
  termination_by sizeOf a
end

/-- Actual Rust unification returns the empty substitution on equal types. -/
theorem unify_reflexive (t : cdsl_hm.Ty) :
    cdsl_hm.unify t t = ok (.Ok { bindings := Slice.new _ }) := by
  rw [cdsl_hm.unify.eq_def]
  simp [cdsl_hm.Ty.Insts.CoreCmpPartialEqTy.eq, equal_type_refines t t,
    sameType, bind_ok, cdsl_hm.Substitution.Insts.CoreDefaultDefault.default,
    alloc.vec.Vec.Insts.CoreDefaultDefault.default, alloc.vec.Vec.new,
    rust_primitives.sequence.seq_empty, bind_ok]
  exact bind_ok _ _

end CDSL.HM.Refinement
