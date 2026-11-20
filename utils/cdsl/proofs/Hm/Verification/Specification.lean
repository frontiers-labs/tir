/- The typing relation is the specification. Rust extraction must refine it. -/
namespace CDSL.HM

inductive Ty where
  | var : Nat → Ty
  | rigid : Nat → Ty
  | atom : Nat → Ty
  | app : Ty → Ty → Ty
  deriving DecidableEq, Repr

abbrev Substitution := Nat → Ty

def Ty.subst (s : Substitution) : Ty → Ty
  | .var v => s v
  | .rigid v => .rigid v
  | .atom c => .atom c
  | .app f a => .app (f.subst s) (a.subst s)

def Ty.free : Ty → List Nat
  | .var v => [v]
  | .rigid _ | .atom _ => []
  | .app f a => f.free ++ a.free

def arrow (a b : Ty) : Ty := .app (.app (.atom 0) a) b

structure Scheme where
  quantified : List Nat
  body : Ty

abbrev Env := List Scheme

def Scheme.free (s : Scheme) : List Nat :=
  s.body.free.filter fun v => !s.quantified.contains v

def Env.free (e : Env) : List Nat := e.flatMap Scheme.free

def Instance (s : Scheme) (t : Ty) : Prop :=
  ∃ sub : Substitution,
    (∀ v, v ∉ s.quantified → sub v = .var v) ∧ s.body.subst sub = t

def Generalizes (e : Env) (t : Ty) (s : Scheme) : Prop :=
  s.body = t ∧ ∀ v, v ∈ s.quantified → v ∈ t.free ∧ v ∉ e.free

inductive Term where
  | var : Nat → Term
  | literal : Nat → Term
  | abs : Term → Term
  | app : Term → Term → Term
  | letIn : Term → Term → Term

/-- Rank-one HM typing, with literal types supplied by the language signature. -/
inductive HasType (signature : Nat → Ty) : Env → Term → Ty → Prop where
  | var {e x s t} : e[x]? = some s → Instance s t → HasType signature e (.var x) t
  | literal {e c} : HasType signature e (.literal c) (signature c)
  | abs {e body a b} :
      HasType signature ({ quantified := [], body := a } :: e) body b →
      HasType signature e (.abs body) (arrow a b)
  | app {e f x a b} :
      HasType signature e f (arrow a b) → HasType signature e x a →
      HasType signature e (.app f x) b
  | letIn {e value body a b s} :
      HasType signature e value a → Generalizes e a s →
      HasType signature (s :: e) body b →
      HasType signature e (.letIn value body) b

def Unifies (s : Substitution) (a b : Ty) : Prop := a.subst s = b.subst s

def MostGeneral (s : Substitution) (a b : Ty) : Prop :=
  Unifies s a b ∧ ∀ other, Unifies other a b →
    ∃ rest, ∀ v, other v = (s v).subst rest

theorem subst_identity (t : Ty) : t.subst Ty.var = t := by
  induction t with
  | var v => rfl
  | rigid v => rfl
  | atom c => rfl
  | app f a hf ha => simp [Ty.subst, hf, ha]

theorem subst_compose (t : Ty) (first second : Substitution) :
    (t.subst first).subst second = t.subst (fun v => (first v).subst second) := by
  induction t with
  | var v => rfl
  | rigid v => rfl
  | atom c => rfl
  | app f a hf ha => simp [Ty.subst, hf, ha]

theorem unifies_reflexive (s : Substitution) (t : Ty) : Unifies s t t := rfl

theorem unifies_preserved (s rest : Substitution) (a b : Ty)
    (h : Unifies s a b) : Unifies (fun v => (s v).subst rest) a b := by
  unfold Unifies at *
  rw [← subst_compose, ← subst_compose, h]

def bind (v : Nat) (t : Ty) : Substitution :=
  fun w => if w = v then t else .var w

theorem subst_unchanged (t : Ty) (s : Substitution)
    (h : ∀ v, v ∈ t.free → s v = .var v) : t.subst s = t := by
  induction t with
  | var v => exact h v (by simp [Ty.free])
  | rigid v => rfl
  | atom c => rfl
  | app f a hf ha =>
    simp only [Ty.subst]
    rw [hf (fun v hv => h v (by simp [Ty.free, hv])),
        ha (fun v hv => h v (by simp [Ty.free, hv]))]

theorem bind_mostGeneral (v : Nat) (t : Ty) (fresh : v ∉ t.free) :
    MostGeneral (bind v t) (.var v) t := by
  have fixed : t.subst (bind v t) = t := by
    apply subst_unchanged
    intro w hw
    have ne : w ≠ v := fun eq => fresh (eq ▸ hw)
    simp [bind, ne]
  constructor
  · simp [Unifies, Ty.subst, bind, fixed]
  · intro other hu
    refine ⟨other, ?_⟩
    intro w
    by_cases eq : w = v
    · subst w
      simpa [Unifies, Ty.subst, bind] using hu
    · simp [bind, eq, Ty.subst]

end CDSL.HM
