import Aeneas

namespace DenseTrieMath

/-- A predicate that stays false after its first false value has one cut. -/
theorem antitone_findIdx {α : Type} (xs : List α) (p : α → Bool)
    (hanti : ∀ (i j : Nat) (hi : i < xs.length) (hj : j < xs.length),
      i ≤ j → p xs[j] = true → p xs[i] = true) :
    ∀ (i : Nat) (hi : i < xs.length),
      p xs[i] = true ↔ i < xs.findIdx (fun x => !p x) := by
  intro i hi
  constructor
  · intro hp
    by_contra hn
    have hki : xs.findIdx (fun x => !p x) ≤ i := by omega
    have hk : xs.findIdx (fun x => !p x) < xs.length := by omega
    have hfirst := List.findIdx_getElem
      (xs := xs) (p := fun x => !p x) (w := hk)
    have htrue := hanti _ i hk hi hki hp
    simp [htrue] at hfirst
  · intro hik
    have hfalse := List.not_of_lt_findIdx (xs := xs)
      (p := fun x => !p x) hik
    have hfalse' := congrArg Bool.not hfalse
    set_option backward.isDefEq.respectTransparency false in
      simpa only [Bool.not_not, Bool.not_false] using hfalse'

/-- Every sorted list has lower and upper query cuts, including empty lists. -/
theorem sorted_partitions {α : Type} [LinearOrder α] (xs : List α)
    (hsorted : xs.Pairwise (· ≤ ·)) (value : α) :
    ∃ low high : Nat, low ≤ high ∧ high ≤ xs.length ∧
      (∀ (i : Nat) (hi : i < xs.length), xs[i] < value ↔ i < low) ∧
      (∀ (i : Nat) (hi : i < xs.length), xs[i] ≤ value ↔ i < high) ∧
      (∀ (i : Nat) (hi : i < xs.length),
        xs[i] = value ↔ low ≤ i ∧ i < high) := by
  have hmono : ∀ (i j : Nat) (hi : i < xs.length) (hj : j < xs.length),
      i ≤ j → xs[i] ≤ xs[j] := by
    intro i j hi hj hij
    rcases Nat.eq_or_lt_of_le hij with heq | hlt
    · subst j
      exact le_refl _
    · exact List.pairwise_iff_getElem.mp hsorted i j hi hj hlt
  let low := xs.findIdx (fun x => !(decide (x < value)))
  let high := xs.findIdx (fun x => !(decide (x ≤ value)))
  have hlo : ∀ (i : Nat) (hi : i < xs.length), xs[i] < value ↔ i < low := by
    have hcut := antitone_findIdx xs (fun x => decide (x < value)) (by
      intro i j hi hj hij hp
      simp only [decide_eq_true_eq] at hp ⊢
      exact lt_of_le_of_lt (hmono i j hi hj hij) hp)
    simpa only [decide_eq_true_eq] using hcut
  have hhi : ∀ (i : Nat) (hi : i < xs.length), xs[i] ≤ value ↔ i < high := by
    have hcut := antitone_findIdx xs (fun x => decide (x ≤ value)) (by
      intro i j hi hj hij hp
      simp only [decide_eq_true_eq] at hp ⊢
      exact le_trans (hmono i j hi hj hij) hp)
    simpa only [decide_eq_true_eq] using hcut
  have hlbound : low ≤ xs.length := List.findIdx_le_length
  have hhbound : high ≤ xs.length := List.findIdx_le_length
  have hlh : low ≤ high := by
    by_contra hn
    have hhl : high < low := by omega
    have hhigh : high < xs.length := by omega
    have hxlt := (hlo high hhigh).mpr hhl
    have hbad := (hhi high hhigh).mp (le_of_lt hxlt)
    omega
  refine ⟨low, high, hlh, hhbound, hlo, hhi, ?_⟩
  intro i hi
  constructor
  · intro heq
    have hnot : ¬xs[i] < value := by simp [heq]
    have hnlow : ¬i < low := fun h => hnot ((hlo i hi).mpr h)
    exact ⟨by omega, (hhi i hi).mp (le_of_eq heq)⟩
  · rintro ⟨hli, hih⟩
    have hnot : ¬xs[i] < value := fun h => by
      have := (hlo i hi).mp h
      omega
    exact le_antisymm ((hhi i hi).mpr hih) (le_of_not_gt hnot)

end DenseTrieMath
