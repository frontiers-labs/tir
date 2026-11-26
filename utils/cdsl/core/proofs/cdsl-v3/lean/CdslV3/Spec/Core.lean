/-!
# CDSL v3 Formal Specification: Core AST, Substitution, and Small-Step Semantics

Standalone, implementation-agnostic specification of the CDSL v3 trusted kernel.
Designed to be imported as-is into TIR to verify BOTH the OCaml prototype (via
`ocaml2lean`) and the production Rust implementation (via Hax -> Lean 4).
-/

namespace CdslV3Spec

/-- Kernel universes: computational `type` (survives erasure) and logical `prop` (erased). -/
inductive SortU where
  | type
  | prop
  deriving DecidableEq, Repr

/-- Kernel types: scalar base types, function arrows, computational inductive (`opt`),
    and a logical `Prop`-sorted inductive (`proof`). -/
inductive Ty where
  | int
  | bool
  | proof
  | opt (elem : Ty)
  | arr (dom cod : Ty)
  deriving DecidableEq, Repr

/-- `proof` lives in `Prop`, and any arrow returning a `Prop` lives in `Prop` (Rocq rule). -/
def Ty.isProp : Ty → Bool
  | .proof => true
  | .arr _ cod => cod.isProp
  | _ => false

/-- Core kernel terms (de Bruijn indexed). -/
inductive Term where
  | var (idx : Nat)
  | litInt (n : Int)
  | litBool (b : Bool)
  | add (a b : Term)
  | lam (dom : Ty) (body : Term)
  | app (fn arg : Term)
  | letE (ty : Ty) (rhs body : Term)
  | none (elemTy : Ty)
  | some (val : Term)
  | matchOpt (scrut noneCase someCase : Term)
  | mkProof
  | matchProof (scrut body : Term)
  deriving DecidableEq, Repr

/-! ## Parallel Renamings and Substitutions -/

abbrev Ren := Nat → Nat
abbrev Sub := Nat → Term

def liftRen (r : Ren) : Ren
  | 0 => 0
  | n + 1 => r n + 1

def rename (r : Ren) : Term → Term
  | .var i => .var (r i)
  | .litInt n => .litInt n
  | .litBool b => .litBool b
  | .add a b => .add (rename r a) (rename r b)
  | .lam dom body => .lam dom (rename (liftRen r) body)
  | .app f a => .app (rename r f) (rename r a)
  | .letE ty rhs body => .letE ty (rename r rhs) (rename (liftRen r) body)
  | .none t => .none t
  | .some v => .some (rename r v)
  | .matchOpt s n c => .matchOpt (rename r s) (rename r n) (rename (liftRen r) c)
  | .mkProof => .mkProof
  | .matchProof s b => .matchProof (rename r s) (rename r b)

def shift1 : Term → Term := rename (· + 1)

def liftSub (σ : Sub) : Sub
  | 0 => .var 0
  | n + 1 => shift1 (σ n)

def subst (σ : Sub) : Term → Term
  | .var i => σ i
  | .litInt n => .litInt n
  | .litBool b => .litBool b
  | .add a b => .add (subst σ a) (subst σ b)
  | .lam dom body => .lam dom (subst (liftSub σ) body)
  | .app f a => .app (subst σ f) (subst σ a)
  | .letE ty rhs body => .letE ty (subst σ rhs) (subst (liftSub σ) body)
  | .none t => .none t
  | .some v => .some (subst σ v)
  | .matchOpt s n c => .matchOpt (subst σ s) (subst σ n) (subst (liftSub σ) c)
  | .mkProof => .mkProof
  | .matchProof s b => .matchProof (subst σ s) (subst σ b)

def subst0 (arg : Term) : Sub
  | 0 => arg
  | n + 1 => .var n

def instantiate (body arg : Term) : Term :=
  subst (subst0 arg) body

/-! ## Values and Call-by-Value Small-Step Semantics -/

inductive IsValue : Term → Prop where
  | litInt (n : Int) : IsValue (.litInt n)
  | litBool (b : Bool) : IsValue (.litBool b)
  | lam (dom : Ty) (body : Term) : IsValue (.lam dom body)
  | none (t : Ty) : IsValue (.none t)
  | some {v : Term} : IsValue v → IsValue (.some v)
  | mkProof : IsValue .mkProof

inductive Step : Term → Term → Prop where
  | addL {a a' b} : Step a a' → Step (.add a b) (.add a' b)
  | addR {n b b'} : Step b b' → Step (.add (.litInt n) b) (.add (.litInt n) b')
  | addBeta (n m : Int) : Step (.add (.litInt n) (.litInt m)) (.litInt (n + m))
  | appL {f f' a} : Step f f' → Step (.app f a) (.app f' a)
  | appR {f a a'} : IsValue f → Step a a' → Step (.app f a) (.app f a')
  | appBeta {dom body v} : IsValue v → Step (.app (.lam dom body) v) (instantiate body v)
  | letRhs {ty rhs rhs' body} : Step rhs rhs' → Step (.letE ty rhs body) (.letE ty rhs' body)
  | letBeta {ty v body} : IsValue v → Step (.letE ty v body) (instantiate body v)
  | someStep {v v'} : Step v v' → Step (.some v) (.some v')
  | matchOptScrut {s s' n c} : Step s s' → Step (.matchOpt s n c) (.matchOpt s' n c)
  | matchOptNone {t n c} : Step (.matchOpt (.none t) n c) n
  | matchOptSome {v n c} : IsValue v → Step (.matchOpt (.some v) n c) (instantiate c v)
  | matchProofScrut {s s' b} : Step s s' → Step (.matchProof s b) (.matchProof s' b)
  | matchProofBeta {b} : Step (.matchProof .mkProof b) b

inductive StepStar : Term → Term → Prop where
  | refl (t : Term) : StepStar t t
  | step {a b c : Term} : Step a b → StepStar b c → StepStar a c

theorem StepStar.trans {a b c : Term} (h1 : StepStar a b) (h2 : StepStar b c) : StepStar a c := by
  induction h1 with
  | refl _ => exact h2
  | step hs _ ih => exact .step hs (ih h2)

end CdslV3Spec
