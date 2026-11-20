import Hm.Verification.Specification

set_option backward.isDefEq.respectTransparency false

namespace CDSL.HM

def compose (newer older : Substitution) : Substitution :=
  fun v => (older v).subst newer

theorem mgu_identity (t : Ty) : MostGeneral Ty.var t t := by
  refine ⟨rfl, ?_⟩
  intro other _
  exact ⟨other, fun _ => rfl⟩

theorem mgu_symmetric (s : Substitution) (a b : Ty) :
    MostGeneral s a b → MostGeneral s b a := by
  intro ⟨sound, principal⟩
  exact ⟨sound.symm, fun other h => principal other h.symm⟩

theorem mgu_wrapped (s : Substitution) (k : Nat) (a b : Ty) :
    MostGeneral s a b → MostGeneral s (.app (.atom k) a) (.app (.atom k) b) := by
  intro ⟨sound, principal⟩
  constructor
  · exact congrArg (Ty.app (.atom k)) sound
  · intro other h
    apply principal other
    exact (Ty.app.inj h).2

/-- Solve the first equation, then solve the second under its substitution. -/
theorem mgu_application (first second : Substitution) (a b x y : Ty)
    (head : MostGeneral first a b)
    (tail : MostGeneral second (x.subst first) (y.subst first)) :
    MostGeneral (compose second first) (.app a x) (.app b y) := by
  constructor
  · have ha := unifies_preserved first second a b head.1
    have hb : Unifies (compose second first) x y := by
      change x.subst (fun v => (first v).subst second) = y.subst (fun v => (first v).subst second)
      rw [← subst_compose, ← subst_compose]
      exact tail.1
    change Ty.app (a.subst (compose second first)) (x.subst (compose second first)) =
      Ty.app (b.subst (compose second first)) (y.subst (compose second first))
    exact congr (congrArg Ty.app ha) hb
  · intro other h
    have parts := Ty.app.inj h
    obtain ⟨rest, factor⟩ := head.2 other parts.1
    have he : other = compose rest first := funext factor
    have htail : Unifies rest (x.subst first) (y.subst first) := by
      have hb := parts.2
      rw [he] at hb
      unfold Unifies
      rw [subst_compose, subst_compose]
      exact hb
    obtain ⟨last, tailFactor⟩ := tail.2 rest htail
    refine ⟨last, ?_⟩
    intro v
    have hr : rest = compose last second := funext tailFactor
    have hv := factor v
    rw [hr] at hv
    change other v = ((first v).subst second).subst last
    rw [subst_compose]
    exact hv

end CDSL.HM
