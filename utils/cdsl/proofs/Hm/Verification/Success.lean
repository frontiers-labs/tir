import Hm.Verification.Substitution

open CoreModels Aeneas
open Aeneas.Std hiding namespace core alloc
open Aeneas.Data.Coinductive
open RustM

namespace CDSL.HM.Refinement

/-- Partial correctness: allocation failure and divergence make no result claim. -/
def Returns {α : Type} (m : RustM α) (p : α → Prop) : Prop :=
  ∀ result, m = ok result → p result

theorem bind_success {α β : Type} (m : RustM α) (f : α → RustM β) (out : β)
    (h : Aeneas.Std.bind m f = ok out) : ∃ value, m = ok value ∧ f value = ok out := by
  cases m using RustM.cases with
  | ret a => exact ⟨a, rfl, by simpa only [bind_ok] using h⟩
  | vis i k => simp only [bind_vis, vis_not_ok] at h
  | div => simp only [bind_div, div_not_ok] at h

def partialEffects : EffectWP RustEffect where
  State := Unit
  wp _ _ _ := True

instance : partialEffects.Monotonic where
  wp_monotonic _ := fun _ => trivial

instance : partialEffects.Conjunctive where
  wp_conj := by intros; trivial

unseal RustM in
theorem returns_dwlp {α : Type} (m : RustM α) (p : α → Prop) :
    Returns m p ↔ DWLP partialEffects m (fun value _ => p value) () := by
  cases m using RustM.cases with
  | ret a =>
    constructor
    · intro h
      exact DWLP.ret_iff.mpr (h a rfl)
    · intro h r eq
      have ha := DWLP.ret_iff.mp h
      have ar := RustM.ok_injective eq
      subst r
      exact ha
  | div =>
    constructor
    · intro _; exact DWLP.div
    · intro _ r eq; exact (div_not_ok eq).elim
  | vis e k =>
    constructor
    · intro _
      apply DWLP.vis
      trivial
    · intro _ result h
      exact (vis_not_ok h).elim

unseal RustM in
theorem returns_admissible {α : Type} (p : α → Prop) :
    Lean.Order.admissible (fun m : RustM α => Returns m p) := by
  simp_rw [returns_dwlp]
  exact DWLP.admissible partialEffects _ ()



theorem returns_binary_admissible {α β γ : Type} (p : α → β → γ → Prop) :
    Lean.Order.admissible (fun f : α → β → RustM γ => ∀ a b, Returns (f a b) (p a b)) := by
  refine Lean.Order.admissible_pi_apply (fun (a : α) (g : β → RustM γ) =>
    ∀ b, Returns (g b) (p a b)) ?_
  intro a
  exact Lean.Order.admissible_pi_apply (fun (b : β) (m : RustM γ) => Returns m (p a b))
    (fun b => returns_admissible (p a b))

end CDSL.HM.Refinement
