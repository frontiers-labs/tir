import CdslV3.Spec.Core

/-!
# CDSL v3 Formal Specification: Kernel Typing, Erasure Soundness, and Conformance Contract

Standalone, implementation-agnostic formal specification of the CDSL v3 kernel.
Import `CdslV3Spec` as-is into TIR to verify BOTH:
- the OCaml prototype (via `tools/ocaml2lean.ml` -> `CdslV3Proofs.OcamlVerify`), and
- the production Rust compiler (via Hax -> Lean 4 -> `RustVerify`).

Provides:
1. Declarative typing (`HasType`) with Rocq's `Prop`-elimination guard
2. Full metatheory: `preservation`, `progress`, `type_soundness`, `prop_elim_into_type_impossible`
3. Target-neutral `MiniML` semantics (`MLExpr`, `MLValue`, `MLStep`, `MLStepStar`)
4. Abstract `KernelImpl` interface and `KernelConformance` contract
5. Reusable refinement theorem `KernelConformance.ofRefines` so any implementation
   (OCaml or Rust) that refines the spec automatically inherits all soundness & erasure theorems!
-/

namespace CdslV3Spec

abbrev Ctx := List Ty

/-- Declarative kernel typing rules (enforcing Rocq's `Prop`-elimination restriction). -/
inductive HasType : Ctx → Term → Ty → Prop where
  | var {Γ i T} :
      Γ[i]? = some T →
      HasType Γ (.var i) T
  | litInt {Γ n} :
      HasType Γ (.litInt n) .int
  | litBool {Γ b} :
      HasType Γ (.litBool b) .bool
  | add {Γ a b} :
      HasType Γ a .int →
      HasType Γ b .int →
      HasType Γ (.add a b) .int
  | lam {Γ dom cod body} :
      HasType (dom :: Γ) body cod →
      HasType Γ (.lam dom body) (.arr dom cod)
  | app {Γ f a dom cod} :
      HasType Γ f (.arr dom cod) →
      HasType Γ a dom →
      HasType Γ (.app f a) cod
  | letE {Γ ty rhs body cod} :
      HasType Γ rhs ty →
      HasType (ty :: Γ) body cod →
      HasType Γ (.letE ty rhs body) cod
  | none {Γ t} :
      HasType Γ (.none t) (.opt t)
  | some {Γ v t} :
      HasType Γ v t →
      HasType Γ (.some v) (.opt t)
  | matchOpt {Γ s n c t res} :
      HasType Γ s (.opt t) →
      HasType Γ n res →
      HasType (t :: Γ) c res →
      HasType Γ (.matchOpt s n c) res
  | mkProof {Γ} :
      HasType Γ .mkProof .proof
  | matchProof {Γ s b res} :
      HasType Γ s .proof →
      HasType Γ b res →
      res.isProp = true →
      HasType Γ (.matchProof s b) res

/-! ## Renaming, Substitution, Preservation, Progress, and Type Soundness -/

def RenTyped (Γ Δ : Ctx) (r : Ren) : Prop :=
  ∀ i T, Γ[i]? = some T → Δ[r i]? = some T

theorem liftRen_typed {Γ Δ : Ctx} {r : Ren} (A : Ty) (hr : RenTyped Γ Δ r) :
    RenTyped (A :: Γ) (A :: Δ) (liftRen r) := by
  intro i T h
  cases i with
  | zero => exact h
  | succ k => exact hr k T h

theorem rename_typing {Γ Δ : Ctx} {t : Term} {T : Ty} {r : Ren}
    (ht : HasType Γ t T) (hr : RenTyped Γ Δ r) :
    HasType Δ (rename r t) T := by
  induction ht generalizing Δ r with
  | var h => exact .var (hr _ _ h)
  | litInt => exact .litInt
  | litBool => exact .litBool
  | add _ _ iha ihb => exact .add (iha hr) (ihb hr)
  | lam _ ih => exact .lam (ih (liftRen_typed _ hr))
  | app _ _ ihf iha => exact .app (ihf hr) (iha hr)
  | letE _ _ ihr ihb => exact .letE (ihr hr) (ihb (liftRen_typed _ hr))
  | none => exact .none
  | some _ ih => exact .some (ih hr)
  | matchOpt _ _ _ ihs ihn ihc =>
      exact .matchOpt (ihs hr) (ihn hr) (ihc (liftRen_typed _ hr))
  | mkProof => exact .mkProof
  | matchProof _ _ hp ihs ihb => exact .matchProof (ihs hr) (ihb hr) hp

theorem weakening {Γ : Ctx} {t : Term} {T A : Ty} (ht : HasType Γ t T) :
    HasType (A :: Γ) (shift1 t) T :=
  rename_typing ht (fun _ _ h => h)

def SubTyped (Γ Δ : Ctx) (σ : Sub) : Prop :=
  ∀ i T, Γ[i]? = some T → HasType Δ (σ i) T

theorem liftSub_typed {Γ Δ : Ctx} {σ : Sub} (A : Ty) (hσ : SubTyped Γ Δ σ) :
    SubTyped (A :: Γ) (A :: Δ) (liftSub σ) := by
  intro i T h
  cases i with
  | zero => exact .var h
  | succ k => exact weakening (hσ k T h)

theorem subst_typing {Γ Δ : Ctx} {t : Term} {T : Ty} {σ : Sub}
    (ht : HasType Γ t T) (hσ : SubTyped Γ Δ σ) :
    HasType Δ (subst σ t) T := by
  induction ht generalizing Δ σ with
  | var h => exact hσ _ _ h
  | litInt => exact .litInt
  | litBool => exact .litBool
  | add _ _ iha ihb => exact .add (iha hσ) (ihb hσ)
  | lam _ ih => exact .lam (ih (liftSub_typed _ hσ))
  | app _ _ ihf iha => exact .app (ihf hσ) (iha hσ)
  | letE _ _ ihr ihb => exact .letE (ihr hσ) (ihb (liftSub_typed _ hσ))
  | none => exact .none
  | some _ ih => exact .some (ih hσ)
  | matchOpt _ _ _ ihs ihn ihc =>
      exact .matchOpt (ihs hσ) (ihn hσ) (ihc (liftSub_typed _ hσ))
  | mkProof => exact .mkProof
  | matchProof _ _ hp ihs ihb => exact .matchProof (ihs hσ) (ihb hσ) hp

theorem instantiate_typing {Γ : Ctx} {body arg : Term} {A B : Ty}
    (hb : HasType (A :: Γ) body B) (ha : HasType Γ arg A) :
    HasType Γ (instantiate body arg) B := by
  apply subst_typing hb
  intro i T h
  cases i with
  | zero => simp at h; subst h; exact ha
  | succ k => exact .var h

theorem preservation {t t' : Term} {T : Ty}
    (ht : HasType [] t T) (hs : Step t t') : HasType [] t' T := by
  induction hs generalizing T with
  | addL _ ih => cases ht with | add ha hb => exact .add (ih ha) hb
  | addR _ ih => cases ht with | add ha hb => exact .add ha (ih hb)
  | addBeta _ _ => cases ht with | add _ _ => exact .litInt
  | appL _ ih => cases ht with | app hf ha => exact .app (ih hf) ha
  | appR _ _ ih => cases ht with | app hf ha => exact .app hf (ih ha)
  | appBeta _ =>
      cases ht with
      | app hf ha =>
          cases hf with
          | lam hb => exact instantiate_typing hb ha
  | letRhs _ ih => cases ht with | letE hr hb => exact .letE (ih hr) hb
  | letBeta _ => cases ht with | letE hr hb => exact instantiate_typing hb hr
  | someStep _ ih => cases ht with | some hv => exact .some (ih hv)
  | matchOptScrut _ ih => cases ht with | matchOpt hs hn hc => exact .matchOpt (ih hs) hn hc
  | matchOptNone => cases ht with | matchOpt _ hn _ => exact hn
  | matchOptSome _ =>
      cases ht with
      | matchOpt hs _ hc =>
          cases hs with | some hv => exact instantiate_typing hc hv
  | matchProofScrut _ ih => cases ht with | matchProof hs hb hp => exact .matchProof (ih hs) hb hp
  | matchProofBeta => cases ht with | matchProof _ hb _ => exact hb

theorem canonical_int {v : Term} (hv : IsValue v) (ht : HasType [] v .int) :
    ∃ n, v = .litInt n := by
  cases hv <;> cases ht; exact ⟨_, rfl⟩

theorem canonical_arr {v : Term} {A B : Ty} (hv : IsValue v) (ht : HasType [] v (.arr A B)) :
    ∃ body, v = .lam A body := by
  cases hv <;> cases ht; exact ⟨_, rfl⟩

theorem canonical_opt {v : Term} {A : Ty} (hv : IsValue v) (ht : HasType [] v (.opt A)) :
    v = .none A ∨ ∃ w, IsValue w ∧ v = .some w := by
  cases hv with
  | none _ => cases ht; exact Or.inl rfl
  | some hw => cases ht; exact Or.inr ⟨_, hw, rfl⟩
  | _ => cases ht

theorem canonical_proof {v : Term} (hv : IsValue v) (ht : HasType [] v .proof) :
    v = .mkProof := by
  cases hv <;> cases ht; rfl

theorem progress {t : Term} {T : Ty} (ht : HasType [] t T) :
    IsValue t ∨ ∃ t', Step t t' := by
  generalize hctx : ([] : Ctx) = Γ at ht
  induction ht with
  | var h => subst hctx; simp at h
  | litInt => exact Or.inl (.litInt _)
  | litBool => exact Or.inl (.litBool _)
  | lam _ => exact Or.inl (.lam _ _)
  | none => exact Or.inl (.none _)
  | mkProof => exact Or.inl .mkProof
  | add ha hb iha ihb =>
      subst hctx
      cases iha rfl with
      | inr h => rcases h with ⟨a', ha'⟩; exact Or.inr ⟨_, .addL ha'⟩
      | inl hva =>
          rcases canonical_int hva ha with ⟨n, rfl⟩
          cases ihb rfl with
          | inr h => rcases h with ⟨b', hb'⟩; exact Or.inr ⟨_, .addR hb'⟩
          | inl hvb =>
              rcases canonical_int hvb hb with ⟨m, rfl⟩
              exact Or.inr ⟨_, .addBeta n m⟩
  | app hf ha ihf iha =>
      subst hctx
      cases ihf rfl with
      | inr h => rcases h with ⟨f', hf'⟩; exact Or.inr ⟨_, .appL hf'⟩
      | inl hvf =>
          rcases canonical_arr hvf hf with ⟨body, rfl⟩
          cases iha rfl with
          | inr h => rcases h with ⟨a', ha'⟩; exact Or.inr ⟨_, .appR hvf ha'⟩
          | inl hva => exact Or.inr ⟨_, .appBeta hva⟩
  | letE hr _ ihr _ =>
      subst hctx
      cases ihr rfl with
      | inr h => rcases h with ⟨r', hr'⟩; exact Or.inr ⟨_, .letRhs hr'⟩
      | inl hvr => exact Or.inr ⟨_, .letBeta hvr⟩
  | some _ ih =>
      subst hctx
      cases ih rfl with
      | inl hv => exact Or.inl (.some hv)
      | inr h => rcases h with ⟨v', hv'⟩; exact Or.inr ⟨_, .someStep hv'⟩
  | matchOpt hs _ _ ihs _ _ =>
      subst hctx
      cases ihs rfl with
      | inr h => rcases h with ⟨s', hs'⟩; exact Or.inr ⟨_, .matchOptScrut hs'⟩
      | inl hvs =>
          cases canonical_opt hvs hs with
          | inl hnone => subst hnone; exact Or.inr ⟨_, .matchOptNone⟩
          | inr hsome => rcases hsome with ⟨w, hvw, rfl⟩; exact Or.inr ⟨_, .matchOptSome hvw⟩
  | matchProof hs _ _ ihs _ =>
      subst hctx
      cases ihs rfl with
      | inr h => rcases h with ⟨s', hs'⟩; exact Or.inr ⟨_, .matchProofScrut hs'⟩
      | inl hvs =>
          have heq := canonical_proof hvs hs
          subst heq
          exact Or.inr ⟨_, .matchProofBeta⟩

/-- Full Type Soundness: a closed well-typed term never reaches a stuck state. -/
theorem type_soundness {t t' : Term} {T : Ty}
    (ht : HasType [] t T) (hsteps : StepStar t t') :
    HasType [] t' T ∧ (IsValue t' ∨ ∃ t'', Step t' t'') := by
  induction hsteps with
  | refl _ => exact ⟨ht, progress ht⟩
  | step hs _ ih => exact ih (preservation ht hs)

/-- Rocq Prop-Elimination Guard Theorem: matching a `Prop` proof into `Type` is impossible. -/
theorem prop_elim_into_type_impossible (s b : Term) {T : Ty} (hcomp : T.isProp = false) :
    ¬ HasType [] (.matchProof s b) T := by
  intro ht
  cases ht with
  | matchProof _ _ hp => rw [hcomp] at hp; contradiction

/-! ## Target-Neutral MiniML Semantics -/

inductive MLExpr where
  | var (idx : Nat)
  | litInt (n : Int)
  | litBool (b : Bool)
  | add (a b : MLExpr)
  | lam (body : MLExpr)
  | app (fn arg : MLExpr)
  | letE (rhs body : MLExpr)
  | none
  | some (val : MLExpr)
  | matchOpt (scrut noneCase someCase : MLExpr)
  | erased
  deriving DecidableEq, Repr

def mlRename (r : Ren) : MLExpr → MLExpr
  | .var i => .var (r i)
  | .litInt n => .litInt n
  | .litBool b => .litBool b
  | .add a b => .add (mlRename r a) (mlRename r b)
  | .lam body => .lam (mlRename (liftRen r) body)
  | .app f a => .app (mlRename r f) (mlRename r a)
  | .letE rhs body => .letE (mlRename r rhs) (mlRename (liftRen r) body)
  | .none => .none
  | .some v => .some (mlRename r v)
  | .matchOpt s n c => .matchOpt (mlRename r s) (mlRename r n) (mlRename (liftRen r) c)
  | .erased => .erased

def mlShift1 : MLExpr → MLExpr := mlRename (· + 1)
abbrev MLSub := Nat → MLExpr

def mlLiftSub (ρ : MLSub) : MLSub
  | 0 => .var 0
  | n + 1 => mlShift1 (ρ n)

def mlSubst (ρ : MLSub) : MLExpr → MLExpr
  | .var i => ρ i
  | .litInt n => .litInt n
  | .litBool b => .litBool b
  | .add a b => .add (mlSubst ρ a) (mlSubst ρ b)
  | .lam body => .lam (mlSubst (mlLiftSub ρ) body)
  | .app f a => .app (mlSubst ρ f) (mlSubst ρ a)
  | .letE rhs body => .letE (mlSubst ρ rhs) (mlSubst (mlLiftSub ρ) body)
  | .none => .none
  | .some v => .some (mlSubst ρ v)
  | .matchOpt s n c => .matchOpt (mlSubst ρ s) (mlSubst ρ n) (mlSubst (mlLiftSub ρ) c)
  | .erased => .erased

def mlSubst0 (arg : MLExpr) : MLSub
  | 0 => arg
  | n + 1 => .var n

def mlInstantiate (body arg : MLExpr) : MLExpr :=
  mlSubst (mlSubst0 arg) body

inductive MLValue : MLExpr → Prop where
  | litInt (n : Int) : MLValue (.litInt n)
  | litBool (b : Bool) : MLValue (.litBool b)
  | lam (body : MLExpr) : MLValue (.lam body)
  | none : MLValue .none
  | some {v : MLExpr} : MLValue v → MLValue (.some v)
  | erased : MLValue .erased

inductive MLStep : MLExpr → MLExpr → Prop where
  | addL {a a' b} : MLStep a a' → MLStep (.add a b) (.add a' b)
  | addR {n b b'} : MLStep b b' → MLStep (.add (.litInt n) b) (.add (.litInt n) b')
  | addBeta (n m : Int) : MLStep (.add (.litInt n) (.litInt m)) (.litInt (n + m))
  | appL {f f' a} : MLStep f f' → MLStep (.app f a) (.app f' a)
  | appR {f a a'} : MLValue f → MLStep a a' → MLStep (.app f a) (.app f a')
  | appBeta {body v} : MLValue v → MLStep (.app (.lam body) v) (mlInstantiate body v)
  | letRhs {rhs rhs' body} : MLStep rhs rhs' → MLStep (.letE rhs body) (.letE rhs' body)
  | letBeta {v body} : MLValue v → MLStep (.letE v body) (mlInstantiate body v)
  | someStep {v v'} : MLStep v v' → MLStep (.some v) (.some v')
  | matchOptScrut {s s' n c} : MLStep s s' → MLStep (.matchOpt s n c) (.matchOpt s' n c)
  | matchOptNone {n c} : MLStep (.matchOpt .none n c) n
  | matchOptSome {v n c} : MLValue v → MLStep (.matchOpt (.some v) n c) (mlInstantiate c v)

inductive MLStepStar : MLExpr → MLExpr → Prop where
  | refl (e : MLExpr) : MLStepStar e e
  | step {a b c : MLExpr} : MLStep a b → MLStepStar b c → MLStepStar a c

theorem MLStepStar.single {a b : MLExpr} (h : MLStep a b) : MLStepStar a b :=
  .step h (.refl b)

theorem MLStepStar.trans {a b c : MLExpr} (h1 : MLStepStar a b) (h2 : MLStepStar b c) :
    MLStepStar a c := by
  induction h1 with
  | refl _ => exact h2
  | step hs _ ih => exact .step hs (ih h2)

/-! ## Reference Specification Typechecker & Eraser -/

def specInferType (Γ : Ctx) : Term → Option Ty
  | .var i => Γ[i]?
  | .litInt _ => some .int
  | .litBool _ => some .bool
  | .add a b =>
      match specInferType Γ a, specInferType Γ b with
      | some .int, some .int => some .int
      | _, _ => none
  | .lam dom body =>
      match specInferType (dom :: Γ) body with
      | some cod => some (.arr dom cod)
      | none => none
  | .app f a =>
      match specInferType Γ f, specInferType Γ a with
      | some (.arr dom cod), some ta => if ta = dom then some cod else none
      | _, _ => none
  | .letE ty rhs body =>
      match specInferType Γ rhs with
      | some tr => if tr = ty then specInferType (ty :: Γ) body else none
      | none => none
  | .none t => some (.opt t)
  | .some v =>
      match specInferType Γ v with
      | some t => some (.opt t)
      | none => none
  | .matchOpt s n c =>
      match specInferType Γ s with
      | some (.opt t) =>
          match specInferType Γ n, specInferType (t :: Γ) c with
          | some rn, some rc => if rn = rc then some rn else none
          | _, _ => none
      | _ => none
  | .mkProof => some .proof
  | .matchProof s b =>
      match specInferType Γ s, specInferType Γ b with
      | some .proof, some res => if res.isProp then some res else none
      | _, _ => none

def specEraseTerm (Γ : Ctx) (t : Term) : MLExpr :=
  match specInferType Γ t with
  | some ty =>
      if ty.isProp then .erased
      else match t with
        | .var i => .var i
        | .litInt n => .litInt n
        | .litBool b => .litBool b
        | .add a b => .add (specEraseTerm Γ a) (specEraseTerm Γ b)
        | .lam dom body => .lam (specEraseTerm (dom :: Γ) body)
        | .app f a => .app (specEraseTerm Γ f) (specEraseTerm Γ a)
        | .letE ty rhs body => .letE (specEraseTerm Γ rhs) (specEraseTerm (ty :: Γ) body)
        | .none _ => .none
        | .some v => .some (specEraseTerm Γ v)
        | .matchOpt s n c =>
            match specInferType Γ s with
            | some (.opt elem) => .matchOpt (specEraseTerm Γ s) (specEraseTerm Γ n) (specEraseTerm (elem :: Γ) c)
            | _ => .erased
        | .mkProof => .erased
        | .matchProof _ _ => .erased
  | none => .erased

theorem specInferType_sound : ∀ Γ t T, specInferType Γ t = some T → HasType Γ t T := by
  intro Γ t
  induction t generalizing Γ with
  | var i => intro T h; exact .var h
  | litInt _ => intro T h; simp [specInferType] at h; subst h; exact .litInt
  | litBool _ => intro T h; simp [specInferType] at h; subst h; exact .litBool
  | add a b iha ihb =>
      intro T h
      simp only [specInferType] at h
      split at h <;> simp_all
      subst h
      exact .add (iha Γ .int (by assumption)) (ihb Γ .int (by assumption))
  | lam dom body ih =>
      intro T h
      simp only [specInferType] at h
      split at h <;> simp_all
      subst h
      exact .lam (ih (dom :: Γ) _ (by assumption))
  | app f a ihf iha =>
      intro T h
      simp only [specInferType] at h
      split at h <;> simp_all
      rename_i dom cod ta hf ha
      by_cases heq : ta = dom
      · simp [heq] at h; subst h; subst heq
        exact .app (ihf Γ (.arr ta cod) hf) (iha Γ ta ha)
      · simp [heq] at h
  | letE ty rhs body ihr ihb =>
      intro T h
      simp only [specInferType] at h
      split at h <;> try contradiction
      rename_i tr hr
      by_cases heq : tr = ty
      · simp [heq] at h; subst heq
        exact .letE (ihr Γ tr hr) (ihb (tr :: Γ) T h)
      · simp [heq] at h
  | none _ => intro T h; simp [specInferType] at h; subst h; exact .none
  | some v ih =>
      intro T h
      simp only [specInferType] at h
      split at h <;> simp_all
      subst h
      exact .some (ih Γ _ (by assumption))
  | matchOpt s n c ihs ihn ihc =>
      intro T h
      simp only [specInferType] at h
      split at h <;> try contradiction
      rename_i t hs
      split at h <;> try contradiction
      rename_i rn rc hn hc
      by_cases heq : rn = rc
      · simp [heq] at h; subst h; subst heq
        exact .matchOpt (ihs Γ (.opt t) hs) (ihn Γ rn hn) (ihc (t :: Γ) rn hc)
      · simp [heq] at h
  | mkProof => intro T h; simp [specInferType] at h; subst h; exact .mkProof
  | matchProof s b ihs ihb =>
      intro T h
      simp only [specInferType] at h
      split at h <;> try contradiction
      rename_i res hs hb
      by_cases hp : res.isProp = true
      · simp [hp] at h; subst h
        exact .matchProof (ihs Γ .proof hs) (ihb Γ res hb) hp
      · simp [hp] at h

theorem specInferType_complete {Γ : Ctx} {t : Term} {T : Ty} (ht : HasType Γ t T) :
    specInferType Γ t = some T := by
  induction ht with
  | var h => exact h
  | litInt => rfl
  | litBool => rfl
  | add _ _ iha ihb => simp [specInferType, iha, ihb]
  | lam _ ih => simp [specInferType, ih]
  | app _ _ ihf iha => simp [specInferType, ihf, iha]
  | letE _ _ ihr ihb => simp [specInferType, ihr, ihb]
  | none => rfl
  | some _ ih => simp [specInferType, ih]
  | matchOpt _ _ _ ihs ihn ihc => simp [specInferType, ihs, ihn, ihc]
  | mkProof => rfl
  | matchProof _ _ hp ihs ihb => simp [specInferType, ihs, ihb, hp]

theorem specEraseTerm_prop {Γ : Ctx} {t : Term} {T : Ty}
    (ht : HasType Γ t T) (hp : T.isProp = true) :
    specEraseTerm Γ t = .erased := by
  unfold specEraseTerm
  rw [specInferType_complete ht]
  simp [hp]

private theorem specErase_add {Γ a b} (ha : HasType Γ a .int) (hb : HasType Γ b .int) :
    specEraseTerm Γ (.add a b) = .add (specEraseTerm Γ a) (specEraseTerm Γ b) := by
  conv => lhs; unfold specEraseTerm
  rw [specInferType_complete (.add ha hb)]; rfl

private theorem specErase_lam {Γ dom cod body} (hb : HasType (dom :: Γ) body cod) (hpf : cod.isProp = false) :
    specEraseTerm Γ (.lam dom body) = .lam (specEraseTerm (dom :: Γ) body) := by
  conv => lhs; unfold specEraseTerm
  rw [specInferType_complete (.lam hb)]; simp [Ty.isProp, hpf]

private theorem specErase_app {Γ f a dom cod}
    (hf : HasType Γ f (.arr dom cod)) (ha : HasType Γ a dom) (hpf : cod.isProp = false) :
    specEraseTerm Γ (.app f a) = .app (specEraseTerm Γ f) (specEraseTerm Γ a) := by
  conv => lhs; unfold specEraseTerm
  rw [specInferType_complete (.app hf ha)]; simp [hpf]

private theorem specErase_letE {Γ ty rhs body cod}
    (hr : HasType Γ rhs ty) (hb : HasType (ty :: Γ) body cod) (hpf : cod.isProp = false) :
    specEraseTerm Γ (.letE ty rhs body) = .letE (specEraseTerm Γ rhs) (specEraseTerm (ty :: Γ) body) := by
  conv => lhs; unfold specEraseTerm
  rw [specInferType_complete (.letE hr hb)]; simp [hpf]

private theorem specErase_some {Γ v t} (hv : HasType Γ v t) :
    specEraseTerm Γ (.some v) = .some (specEraseTerm Γ v) := by
  conv => lhs; unfold specEraseTerm
  rw [specInferType_complete (.some hv)]; rfl

private theorem specErase_matchOpt {Γ s n c t res}
    (hs : HasType Γ s (.opt t)) (hn : HasType Γ n res) (hc : HasType (t :: Γ) c res)
    (hpf : res.isProp = false) :
    specEraseTerm Γ (.matchOpt s n c) = .matchOpt (specEraseTerm Γ s) (specEraseTerm Γ n) (specEraseTerm (t :: Γ) c) := by
  conv => lhs; unfold specEraseTerm
  rw [specInferType_complete (.matchOpt hs hn hc)]
  simp only [hpf, Bool.false_eq_true, ↓reduceIte]
  rw [specInferType_complete hs]

private theorem specErase_value {v : Term} {T : Ty} (hv : IsValue v) (ht : HasType [] v T) :
    MLValue (specEraseTerm [] v) := by
  induction hv generalizing T with
  | litInt n => cases ht; exact .litInt n
  | litBool b => cases ht; exact .litBool b
  | lam dom body =>
      cases ht with
      | lam hb =>
          rename_i cod
          by_cases hp : cod.isProp = true
          · rw [specEraseTerm_prop (.lam hb) (by simp [Ty.isProp, hp])]; exact .erased
          · rw [specErase_lam hb (Bool.eq_false_iff.mpr hp)]; exact .lam _
  | none _ => cases ht; exact .none
  | some _ ih =>
      cases ht with
      | some ht' => rw [specErase_some ht']; exact .some (ih ht')
  | mkProof => cases ht; exact .erased

private theorem specErase_rename {Γ Δ : Ctx} {t : Term} {T : Ty} {r : Ren}
    (ht : HasType Γ t T) (hr : RenTyped Γ Δ r) :
    specEraseTerm Δ (rename r t) = mlRename r (specEraseTerm Γ t) := by
  induction ht generalizing Δ r with
  | var h =>
      rename_i Γ₀ i T₀
      by_cases hp : T₀.isProp = true
      · rw [specEraseTerm_prop (rename_typing (.var h) hr) hp, specEraseTerm_prop (.var h) hp]; rfl
      · have hpf := Bool.eq_false_iff.mpr hp
        simp [rename, specEraseTerm, specInferType, h, hr i T₀ h, hpf, mlRename]
  | litInt => rfl
  | litBool => rfl
  | add ha hb iha ihb =>
      simp only [rename]
      rw [specErase_add (rename_typing ha hr) (rename_typing hb hr), specErase_add ha hb]
      simp only [mlRename, iha hr, ihb hr]
  | lam hb ih =>
      rename_i Γ₀ dom cod body
      by_cases hp : cod.isProp = true
      · have hp' : (Ty.arr dom cod).isProp = true := by simp [Ty.isProp, hp]
        rw [specEraseTerm_prop (rename_typing (.lam hb) hr) hp', specEraseTerm_prop (.lam hb) hp']; rfl
      · have hpf := Bool.eq_false_iff.mpr hp
        simp only [rename]
        rw [specErase_lam (rename_typing hb (liftRen_typed dom hr)) hpf, specErase_lam hb hpf]
        simp only [mlRename, ih (liftRen_typed dom hr)]
  | app hf ha ihf iha =>
      rename_i Γ₀ f a dom cod
      by_cases hp : cod.isProp = true
      · rw [specEraseTerm_prop (rename_typing (.app hf ha) hr) hp, specEraseTerm_prop (.app hf ha) hp]; rfl
      · have hpf := Bool.eq_false_iff.mpr hp
        simp only [rename]
        rw [specErase_app (rename_typing hf hr) (rename_typing ha hr) hpf, specErase_app hf ha hpf]
        simp only [mlRename, ihf hr, iha hr]
  | letE hrhs hb ihr ihb =>
      rename_i Γ₀ ty rhs body cod
      by_cases hp : cod.isProp = true
      · rw [specEraseTerm_prop (rename_typing (.letE hrhs hb) hr) hp, specEraseTerm_prop (.letE hrhs hb) hp]; rfl
      · have hpf := Bool.eq_false_iff.mpr hp
        simp only [rename]
        rw [specErase_letE (rename_typing hrhs hr) (rename_typing hb (liftRen_typed ty hr)) hpf,
            specErase_letE hrhs hb hpf]
        simp only [mlRename, ihr hr, ihb (liftRen_typed ty hr)]
  | none => rfl
  | some hv ih =>
      simp only [rename]
      rw [specErase_some (rename_typing hv hr), specErase_some hv]
      simp only [mlRename, ih hr]
  | matchOpt hs hn hc ihs ihn ihc =>
      rename_i Γ₀ s n c t₀ res
      by_cases hp : res.isProp = true
      · rw [specEraseTerm_prop (rename_typing (.matchOpt hs hn hc) hr) hp,
            specEraseTerm_prop (.matchOpt hs hn hc) hp]; rfl
      · have hpf := Bool.eq_false_iff.mpr hp
        simp only [rename]
        rw [specErase_matchOpt (rename_typing hs hr) (rename_typing hn hr)
              (rename_typing hc (liftRen_typed t₀ hr)) hpf,
            specErase_matchOpt hs hn hc hpf]
        simp only [mlRename, ihs hr, ihn hr, ihc (liftRen_typed t₀ hr)]
  | mkProof => rfl
  | matchProof hs hb hp _ _ =>
      rw [specEraseTerm_prop (rename_typing (.matchProof hs hb hp) hr) hp,
          specEraseTerm_prop (.matchProof hs hb hp) hp]
      rfl

private def SubErased (Γ Δ : Ctx) (σ : Sub) (ρ : MLSub) : Prop :=
  ∀ i T, Γ[i]? = some T → T.isProp = false → specEraseTerm Δ (σ i) = ρ i

private theorem liftSub_erased {Γ Δ : Ctx} {σ : Sub} {ρ : MLSub} (A : Ty)
    (hσ : SubTyped Γ Δ σ) (he : SubErased Γ Δ σ ρ) :
    SubErased (A :: Γ) (A :: Δ) (liftSub σ) (mlLiftSub ρ) := by
  intro i T h hcomp
  cases i with
  | zero =>
      simp at h; subst h
      simp [liftSub, mlLiftSub, specEraseTerm, specInferType, hcomp]
  | succ k =>
      simp at h
      simp only [liftSub, mlLiftSub, shift1, mlShift1]
      have hren : RenTyped Δ (A :: Δ) (· + 1) := fun _ _ h' => h'
      rw [specErase_rename (hσ k T h) hren, he k T h hcomp]

private theorem specErase_subst {Γ Δ : Ctx} {t : Term} {T : Ty} {σ : Sub} {ρ : MLSub}
    (ht : HasType Γ t T) (hσ : SubTyped Γ Δ σ) (he : SubErased Γ Δ σ ρ) :
    specEraseTerm Δ (subst σ t) = mlSubst ρ (specEraseTerm Γ t) := by
  induction ht generalizing Δ σ ρ with
  | var h =>
      rename_i Γ₀ i T₀
      by_cases hp : T₀.isProp = true
      · rw [specEraseTerm_prop (subst_typing (.var h) hσ) hp, specEraseTerm_prop (.var h) hp]; rfl
      · have hpf := Bool.eq_false_iff.mpr hp
        simp only [subst, specEraseTerm, specInferType, h, hpf, Bool.false_eq_true, ↓reduceIte, mlSubst]
        exact he i T₀ h hpf
  | litInt => rfl
  | litBool => rfl
  | add ha hb iha ihb =>
      simp only [subst]
      rw [specErase_add (subst_typing ha hσ) (subst_typing hb hσ), specErase_add ha hb]
      simp only [mlSubst, iha hσ he, ihb hσ he]
  | lam hb ih =>
      rename_i Γ₀ dom cod body
      by_cases hp : cod.isProp = true
      · have hp' : (Ty.arr dom cod).isProp = true := by simp [Ty.isProp, hp]
        rw [specEraseTerm_prop (subst_typing (.lam hb) hσ) hp', specEraseTerm_prop (.lam hb) hp']; rfl
      · have hpf := Bool.eq_false_iff.mpr hp
        simp only [subst]
        rw [specErase_lam (subst_typing hb (liftSub_typed dom hσ)) hpf, specErase_lam hb hpf]
        simp only [mlSubst, ih (liftSub_typed dom hσ) (liftSub_erased dom hσ he)]
  | app hf ha ihf iha =>
      rename_i Γ₀ f a dom cod
      by_cases hp : cod.isProp = true
      · rw [specEraseTerm_prop (subst_typing (.app hf ha) hσ) hp, specEraseTerm_prop (.app hf ha) hp]; rfl
      · have hpf := Bool.eq_false_iff.mpr hp
        simp only [subst]
        rw [specErase_app (subst_typing hf hσ) (subst_typing ha hσ) hpf, specErase_app hf ha hpf]
        simp only [mlSubst, ihf hσ he, iha hσ he]
  | letE hrhs hb ihr ihb =>
      rename_i Γ₀ ty rhs body cod
      by_cases hp : cod.isProp = true
      · rw [specEraseTerm_prop (subst_typing (.letE hrhs hb) hσ) hp, specEraseTerm_prop (.letE hrhs hb) hp]; rfl
      · have hpf := Bool.eq_false_iff.mpr hp
        simp only [subst]
        rw [specErase_letE (subst_typing hrhs hσ) (subst_typing hb (liftSub_typed ty hσ)) hpf,
            specErase_letE hrhs hb hpf]
        simp only [mlSubst, ihr hσ he, ihb (liftSub_typed ty hσ) (liftSub_erased ty hσ he)]
  | none => rfl
  | some hv ih =>
      simp only [subst]
      rw [specErase_some (subst_typing hv hσ), specErase_some hv]
      simp only [mlSubst, ih hσ he]
  | matchOpt hs hn hc ihs ihn ihc =>
      rename_i Γ₀ s n c t₀ res
      by_cases hp : res.isProp = true
      · rw [specEraseTerm_prop (subst_typing (.matchOpt hs hn hc) hσ) hp,
            specEraseTerm_prop (.matchOpt hs hn hc) hp]; rfl
      · have hpf := Bool.eq_false_iff.mpr hp
        simp only [subst]
        rw [specErase_matchOpt (subst_typing hs hσ) (subst_typing hn hσ)
              (subst_typing hc (liftSub_typed t₀ hσ)) hpf,
            specErase_matchOpt hs hn hc hpf]
        simp only [mlSubst, ihs hσ he, ihn hσ he,
          ihc (liftSub_typed t₀ hσ) (liftSub_erased t₀ hσ he)]
  | mkProof => rfl
  | matchProof hs hb hp _ _ =>
      rw [specEraseTerm_prop (subst_typing (.matchProof hs hb hp) hσ) hp,
          specEraseTerm_prop (.matchProof hs hb hp) hp]
      rfl

private theorem specErase_instantiate {Γ : Ctx} {body arg : Term} {A B : Ty}
    (hb : HasType (A :: Γ) body B) (ha : HasType Γ arg A) :
    specEraseTerm Γ (instantiate body arg) =
      mlInstantiate (specEraseTerm (A :: Γ) body) (specEraseTerm Γ arg) := by
  unfold instantiate mlInstantiate
  apply specErase_subst hb
  · intro i T h
    cases i with
    | zero => simp at h; subst h; exact ha
    | succ k => exact .var h
  · intro i T h hcomp
    cases i with
    | zero => rfl
    | succ k =>
        simp at h
        simp [subst0, mlSubst0, specEraseTerm, specInferType, h, hcomp]

private theorem mlStar_addL {a a' b} (h : MLStepStar a a') : MLStepStar (.add a b) (.add a' b) := by
  induction h with | refl _ => exact .refl _ | step hs _ ih => exact .step (.addL hs) ih

private theorem mlStar_addR {n b b'} (h : MLStepStar b b') :
    MLStepStar (.add (.litInt n) b) (.add (.litInt n) b') := by
  induction h with | refl _ => exact .refl _ | step hs _ ih => exact .step (.addR hs) ih

private theorem mlStar_appL {f f' a} (h : MLStepStar f f') : MLStepStar (.app f a) (.app f' a) := by
  induction h with | refl _ => exact .refl _ | step hs _ ih => exact .step (.appL hs) ih

private theorem mlStar_appR {f a a'} (hf : MLValue f) (h : MLStepStar a a') :
    MLStepStar (.app f a) (.app f a') := by
  induction h with | refl _ => exact .refl _ | step hs _ ih => exact .step (.appR hf hs) ih

private theorem mlStar_letRhs {r r' b} (h : MLStepStar r r') : MLStepStar (.letE r b) (.letE r' b) := by
  induction h with | refl _ => exact .refl _ | step hs _ ih => exact .step (.letRhs hs) ih

private theorem mlStar_some {v v'} (h : MLStepStar v v') : MLStepStar (.some v) (.some v') := by
  induction h with | refl _ => exact .refl _ | step hs _ ih => exact .step (.someStep hs) ih

private theorem mlStar_matchOptScrut {s s' n c} (h : MLStepStar s s') :
    MLStepStar (.matchOpt s n c) (.matchOpt s' n c) := by
  induction h with | refl _ => exact .refl _ | step hs _ ih => exact .step (.matchOptScrut hs) ih

theorem specEraseTerm_simulation_step {t t' : Term} {T : Ty}
    (ht : HasType [] t T) (hs : Step t t') :
    MLStepStar (specEraseTerm [] t) (specEraseTerm [] t') := by
  induction hs generalizing T with
  | addL ha' ih =>
      cases ht with | add ha hb =>
        rw [specErase_add ha hb, specErase_add (preservation ha ha') hb]
        exact mlStar_addL (ih ha)
  | addR hb' ih =>
      cases ht with | add ha hb =>
        rw [specErase_add ha hb, specErase_add ha (preservation hb hb')]
        exact mlStar_addR (ih hb)
  | addBeta n m =>
      cases ht with | add ha hb =>
        rw [specErase_add ha hb]
        exact .single (.addBeta n m)
  | appL hf' ih =>
      cases ht with | app hf ha =>
        by_cases hp : T.isProp = true
        · rw [specEraseTerm_prop (.app hf ha) hp, specEraseTerm_prop (.app (preservation hf hf') ha) hp]
          exact .refl .erased
        · have hpf := Bool.eq_false_iff.mpr hp
          rw [specErase_app hf ha hpf, specErase_app (preservation hf hf') ha hpf]
          exact mlStar_appL (ih hf)
  | appR hvf ha' ih =>
      cases ht with | app hf ha =>
        by_cases hp : T.isProp = true
        · rw [specEraseTerm_prop (.app hf ha) hp, specEraseTerm_prop (.app hf (preservation ha ha')) hp]
          exact .refl .erased
        · have hpf := Bool.eq_false_iff.mpr hp
          rw [specErase_app hf ha hpf, specErase_app hf (preservation ha ha') hpf]
          exact mlStar_appR (specErase_value hvf hf) (ih ha)
  | appBeta hv =>
      cases ht with | app hf ha =>
        cases hf with | lam hb =>
          by_cases hp : T.isProp = true
          · rw [specEraseTerm_prop (.app (.lam hb) ha) hp, specEraseTerm_prop (instantiate_typing hb ha) hp]
            exact .refl .erased
          · have hpf := Bool.eq_false_iff.mpr hp
            rw [specErase_app (.lam hb) ha hpf, specErase_lam hb hpf, specErase_instantiate hb ha]
            exact .single (.appBeta (specErase_value hv ha))
  | letRhs hr' ih =>
      cases ht with | letE hr hb =>
        by_cases hp : T.isProp = true
        · rw [specEraseTerm_prop (.letE hr hb) hp, specEraseTerm_prop (.letE (preservation hr hr') hb) hp]
          exact .refl .erased
        · have hpf := Bool.eq_false_iff.mpr hp
          rw [specErase_letE hr hb hpf, specErase_letE (preservation hr hr') hb hpf]
          exact mlStar_letRhs (ih hr)
  | letBeta hv =>
      cases ht with | letE hr hb =>
        by_cases hp : T.isProp = true
        · rw [specEraseTerm_prop (.letE hr hb) hp, specEraseTerm_prop (instantiate_typing hb hr) hp]
          exact .refl .erased
        · have hpf := Bool.eq_false_iff.mpr hp
          rw [specErase_letE hr hb hpf, specErase_instantiate hb hr]
          exact .single (.letBeta (specErase_value hv hr))
  | someStep hv' ih =>
      cases ht with | some hv =>
        rw [specErase_some hv, specErase_some (preservation hv hv')]
        exact mlStar_some (ih hv)
  | matchOptScrut hs' ih =>
      cases ht with | matchOpt hsc hn hc =>
        by_cases hp : T.isProp = true
        · rw [specEraseTerm_prop (.matchOpt hsc hn hc) hp,
              specEraseTerm_prop (.matchOpt (preservation hsc hs') hn hc) hp]
          exact .refl .erased
        · have hpf := Bool.eq_false_iff.mpr hp
          rw [specErase_matchOpt hsc hn hc hpf, specErase_matchOpt (preservation hsc hs') hn hc hpf]
          exact mlStar_matchOptScrut (ih hsc)
  | matchOptNone =>
      cases ht with | matchOpt hsc hn hc =>
        by_cases hp : T.isProp = true
        · rw [specEraseTerm_prop (.matchOpt hsc hn hc) hp, specEraseTerm_prop hn hp]
          exact .refl .erased
        · have hpf := Bool.eq_false_iff.mpr hp
          rw [specErase_matchOpt hsc hn hc hpf]
          exact .single .matchOptNone
  | matchOptSome hv =>
      cases ht with | matchOpt hsc hn hc =>
        cases hsc with | some hv_ty =>
          by_cases hp : T.isProp = true
          · rw [specEraseTerm_prop (.matchOpt (.some hv_ty) hn hc) hp,
                specEraseTerm_prop (instantiate_typing hc hv_ty) hp]
            exact .refl .erased
          · have hpf := Bool.eq_false_iff.mpr hp
            rw [specErase_matchOpt (.some hv_ty) hn hc hpf, specErase_some hv_ty,
                specErase_instantiate hc hv_ty]
            exact .single (.matchOptSome (specErase_value hv hv_ty))
  | matchProofScrut hs' _ =>
      cases ht with | matchProof hsc hb hp =>
        rw [specEraseTerm_prop (.matchProof hsc hb hp) hp,
            specEraseTerm_prop (.matchProof (preservation hsc hs') hb hp) hp]
        exact .refl .erased
  | matchProofBeta =>
      cases ht with | matchProof hsc hb hp =>
        rw [specEraseTerm_prop (.matchProof hsc hb hp) hp, specEraseTerm_prop hb hp]
        exact .refl .erased

theorem specEraseTerm_simulation {t t' : Term} {T : Ty}
    (ht : HasType [] t T) (hsteps : StepStar t t') :
    MLStepStar (specEraseTerm [] t) (specEraseTerm [] t') := by
  induction hsteps with
  | refl _ => exact .refl _
  | step hs _ ih => exact (specEraseTerm_simulation_step ht hs).trans (ih (preservation ht hs))

/-! ## Implementation-Agnostic Kernel Conformance Contract -/

/-- Abstract interface that ANY CDSL v3 kernel implementation (OCaml or Hax-extracted Rust) provides. -/
structure KernelImpl where
  propElimAllowed : Bool → Bool → Bool
  inferType       : Ctx → Term → Option Ty
  eraseTerm       : Ctx → Term → MLExpr

/-- **Formal Kernel Conformance Contract:**
    Proving `KernelConformance impl` certifies that `impl` satisfies:
    1. Rocq's `Prop`-elimination guard,
    2. Soundness and completeness of typechecking w.r.t. `HasType`,
    3. Total erasure of `Prop`-typed terms,
    4. Multi-step operational simulation under erasure, and
    5. Exact `Int` and `Bool` value preservation across erasure! -/
structure KernelConformance (impl : KernelImpl) : Prop where
  prop_elim_guard_sound :
    ∀ b, impl.propElimAllowed true b = true → b = true
  infer_sound :
    ∀ Γ t T, impl.inferType Γ t = some T → HasType Γ t T
  infer_complete :
    ∀ Γ t T, HasType Γ t T → impl.inferType Γ t = some T
  erase_prop :
    ∀ Γ t T, HasType Γ t T → T.isProp = true → impl.eraseTerm Γ t = .erased
  erase_simulation :
    ∀ t t' T, HasType [] t T → StepStar t t' →
      MLStepStar (impl.eraseTerm [] t) (impl.eraseTerm [] t')
  erase_preserves_int :
    ∀ t n, HasType [] t .int → StepStar t (.litInt n) →
      MLStepStar (impl.eraseTerm [] t) (.litInt n)
  erase_preserves_bool :
    ∀ t b, HasType [] t .bool → StepStar t (.litBool b) →
      MLStepStar (impl.eraseTerm [] t) (.litBool b)

/-- **Reusable Refinement Theorem:**
    Any implementation (OCaml prototype or Hax-extracted Rust) that proves its
    `propElimAllowed`, `inferType`, and `eraseTerm` agree with `CdslV3Spec`
    automatically discharges ALL 7 obligations of `KernelConformance`! -/
theorem KernelConformance.ofRefines (impl : KernelImpl)
    (hguard : ∀ s r, impl.propElimAllowed s r = (if s then r else true))
    (hinfer : ∀ Γ t, impl.inferType Γ t = specInferType Γ t)
    (herase : ∀ Γ t, impl.eraseTerm Γ t = specEraseTerm Γ t) :
    KernelConformance impl where
  prop_elim_guard_sound := by
    intro b h
    rw [hguard] at h
    simpa using h
  infer_sound := by
    intro Γ t T h
    rw [hinfer] at h
    exact specInferType_sound Γ t T h
  infer_complete := by
    intro Γ t T ht
    rw [hinfer]
    exact specInferType_complete ht
  erase_prop := by
    intro Γ t T ht hp
    rw [herase]
    exact specEraseTerm_prop ht hp
  erase_simulation := by
    intro t t' T ht hs
    rw [herase, herase]
    exact specEraseTerm_simulation ht hs
  erase_preserves_int := by
    intro t n ht hs
    rw [herase]
    exact specEraseTerm_simulation ht hs
  erase_preserves_bool := by
    intro t b ht hs
    rw [herase]
    exact specEraseTerm_simulation ht hs

end CdslV3Spec
