import Indexmap.Extraction

open Aeneas.Std hiding namespace core alloc
open RustM CoreModels
open CoreModels.std.collections.hash.map
open Indexmap.DependencyContracts

namespace Indexmap

open tir_adt.hashmap.indexmap

/-- The observable insertion-order sequence represented by the parallel Vecs. -/
def items {K V : Type} (m : IndexMap K V) : List (K × V) := m.keys.val.zip m.values.val

/-- Every key occurs once, and the hash index is exactly its Vec position. -/
structure Valid {K V : Type} (m : IndexMap K V) : Prop where
  lengths : m.keys.val.length = m.values.val.length
  unique : m.keys.val.Nodup
  indexed : ∀ k i, m.indices.lookup k = some i ↔ m.keys.val[i.val]? = some k

variable {K V : Type}

private theorem usize_ext {i j : Usize} (h : i.val = j.val) : i = j := by
  exact UScalar.eq_of_val_eq h

private theorem value_exists (m : IndexMap K V) (h : Valid m) (k : K) (i : Usize)
    (hi : m.indices.lookup k = some i) : ∃ v, m.values.val[i.val]? = some v := by
  have hk := (h.indexed k i).mp hi
  have hb : i.val < m.keys.val.length := List.getElem?_eq_some_iff.mp hk |>.1
  have hv : i.val < m.values.val.length := by have := h.lengths; omega
  exact ⟨m.values.val[i.val], List.getElem?_eq_getElem hv⟩

theorem new_valid (eq : core.cmp.Eq K) (hash : core.hash.Hash K) :
    ∃ m : IndexMap K V,
      IndexMap.new V eq hash = ok m ∧ Valid m ∧ items m = [] := by
  let m : IndexMap K V := ⟨⟨fun _ => none⟩, Slice.new K, Slice.new V⟩
  refine ⟨m, ?_, ?_, rfl⟩
  · rfl
  · constructor
    · rfl
    · simp [m, Slice.new]
    · intro k i
      simp [m, Slice.new]

/-- Lookup returns exactly the value at the key's unique sequence position. -/
theorem get_correct (eq : core.cmp.Eq K) (hash : core.hash.Hash K)
    (borrow : core.borrow.Borrow K K) (hborrow : ∀ k, borrow.borrow k = ok k)
    (hkeys : LawfulKeys eq hash)
    (m : IndexMap K V) (h : Valid m) (k : K) :
    ∃ value, IndexMap.get eq hash borrow hash eq m k = ok value ∧
      (value = none ↔ k ∉ m.keys.val) ∧
      (∀ v, value = some v ↔ (k, v) ∈ items m) := by
  cases hi : m.indices.lookup k with
  | none =>
    refine ⟨none, ?_, ?_, ?_⟩
    · simp [IndexMap.get, HashMap.get_identity eq hash _ borrow hborrow hkeys, hi,
        core.option.Option.map]
    · simp only [true_iff]
      intro hk
      obtain ⟨n, hn⟩ := List.mem_iff_getElem?.mp hk
      have hb : n < m.keys.val.length := List.getElem?_eq_some_iff.mp hn |>.1
      let i : Usize := Usize.ofNatCore n (by have := m.keys.property; scalar_tac)
      have hv : i.val = n := by simp [i]
      have := (h.indexed k i).mpr (by simpa [hv] using hn)
      simp [hi] at this
    · intro v
      simp only [reduceCtorEq, false_iff]
      intro hm
      have : k ∈ m.keys.val := List.mem_of_getElem? (List.getElem?_zip_eq_some.mp
        (List.mem_iff_getElem?.mp hm).choose_spec).1
      obtain ⟨n, hn⟩ := List.mem_iff_getElem?.mp this
      have hb : n < m.keys.val.length := List.getElem?_eq_some_iff.mp hn |>.1
      let i : Usize := Usize.ofNatCore n (by have := m.keys.property; scalar_tac)
      have := (h.indexed k i).mpr (by simpa [i] using hn)
      simp [hi] at this
  | some i =>
    obtain ⟨v, hv⟩ := value_exists m h k i hi
    refine ⟨some v, ?_, ?_, ?_⟩
    · simp [IndexMap.get, HashMap.get_identity eq hash _ borrow hborrow hkeys, hi,
        core.option.Option.map,
        tir_adt.hashmap.indexmap.IndexMap.get.closure.Insts.CoreOpsFunctionFnOnceTupleSharedUsizeSharedV.call_once,
        vec_index m.values i v hv]
    · have hk := List.mem_of_getElem? ((h.indexed k i).mp hi)
      simp [hk]
    · intro w
      constructor
      · intro hw
        cases hw
        exact List.mem_iff_getElem?.mpr ⟨i.val,
          List.getElem?_zip_eq_some.mpr ⟨(h.indexed k i).mp hi, hv⟩⟩
      · intro hm
        obtain ⟨n, hn⟩ := List.mem_iff_getElem?.mp hm
        obtain ⟨hk, hw⟩ := List.getElem?_zip_eq_some.mp hn
        have hb : n < m.keys.val.length := List.getElem?_eq_some_iff.mp hk |>.1
        let j : Usize := Usize.ofNatCore n (by have := m.keys.property; scalar_tac)
        have hj := (h.indexed k j).mpr (by simpa [j] using hk)
        have ij : i = j := Option.some.inj (hi.symm.trans hj)
        have vn : i.val = n := by simpa [j] using congrArg UScalar.val ij
        rw [← vn, hv] at hw
        exact hw

private theorem insert_occupied (clone : core.clone.Clone K)
    (eq : core.cmp.Eq K) (hash : core.hash.Hash K)
    (m : IndexMap K V) (k : K) (v old : V) (i : Usize)
    (hi : m.indices.lookup k = some i) (hv : m.values.val[i.val]? = some old)
    (hkeys : LawfulKeys eq hash) (hentry : HashMap.CanEntry m.indices k) :
    IndexMap.insert clone eq hash m k v =
      ok (some old, { m with values := Slice.set m.values i v }) := by
  simp [IndexMap.insert, HashMap.entry, hkeys, hentry, hi, OccupiedEntry.get,
    vec_index_mut m.values i old hv, core.mem.replace, Aeneas.Std.core.mem.replace]
  rfl

private theorem insert_vacant (clone : core.clone.Clone K)
    (eq : core.cmp.Eq K) (hash : core.hash.Hash K)
    (m : IndexMap K V) (k : K) (v : V)
    (hi : m.indices.lookup k = none) (hc : clone.clone k = ok k)
    (hkeys : LawfulKeys eq hash) (hentry : HashMap.CanEntry m.indices k)
    (hpushk : CanPush m.keys k) (hpushv : CanPush m.values v)
    (hk : m.keys.val.length < Usize.max) (hv : m.values.val.length < Usize.max) :
    IndexMap.insert clone eq hash m k v =
      ok (none, ⟨m.indices.set k (Slice.len m.values),
        ⟨m.keys.val ++ [k], by simp; omega⟩,
        ⟨m.values.val ++ [v], by simp; omega⟩⟩) := by
  simp [IndexMap.insert, HashMap.entry, hkeys, hentry, hi, VacantEntry.key, hc,
    vec_push m.keys k hk hpushk, vec_push m.values v hv hpushv, alloc.vec.Vec.len,
    rust_primitives.sequence.seq_len, VacantEntry.insert]

private theorem valid_occupied (m : IndexMap K V) (h : Valid m) (i : Usize) (v : V) :
    Valid { m with values := Slice.set m.values i v } := by
  constructor
  · simpa [Slice.set, Slice.setAtNat] using h.lengths
  · exact h.unique
  · exact h.indexed

private theorem lookup_none_iff (m : IndexMap K V) (h : Valid m) (k : K) :
    m.indices.lookup k = none ↔ k ∉ m.keys.val := by
  constructor
  · intro hi hk
    obtain ⟨n, hn⟩ := List.mem_iff_getElem?.mp hk
    have hb : n < m.keys.val.length := List.getElem?_eq_some_iff.mp hn |>.1
    let i : Usize := Usize.ofNatCore n (by have := m.keys.property; scalar_tac)
    have := (h.indexed k i).mpr (by simpa [i] using hn)
    simp [hi] at this
  · intro hk
    cases hi : m.indices.lookup k with
    | none => rfl
    | some i => exact False.elim (hk (List.mem_of_getElem? ((h.indexed k i).mp hi)))

private theorem valid_appended (m : IndexMap K V) (h : Valid m) (k : K) (v : V)
    (hi : m.indices.lookup k = none)
    (hk : m.keys.val.length < Usize.max) (hv : m.values.val.length < Usize.max) :
    Valid (⟨m.indices.set k (Slice.len m.values),
      ⟨m.keys.val ++ [k], by simp; omega⟩,
      ⟨m.values.val ++ [v], by simp; omega⟩⟩ : IndexMap K V) := by
  classical
  have hnot := (lookup_none_iff m h k).mp hi
  constructor
  · simpa using h.lengths
  · simp only [List.nodup_append, h.unique, List.nodup_singleton, true_and,
      List.mem_singleton, forall_eq]
    intro a ha heq
    exact hnot (heq ▸ ha)
  · intro q j
    change (if q = k then some (Slice.len m.values) else m.indices.lookup q) = some j ↔
      (m.keys.val ++ [k])[j.val]? = some q
    by_cases hq : q = k
    · subst q
      simp only [ite_true, Option.some.injEq]
      constructor
      · intro hj
        subst j
        simp [← h.lengths]
      · intro hj
        have hjn : j.val = m.keys.val.length := by
          by_cases hb : j.val < m.keys.val.length
          · rw [List.getElem?_append_left hb] at hj
            exact False.elim (hnot (List.mem_of_getElem? hj))
          · rw [List.getElem?_append_right (by omega)] at hj
            have : j.val - m.keys.val.length = 0 := by
              cases he : j.val - m.keys.val.length with
              | zero => rfl
              | succ n => simp [he] at hj
            omega
        apply usize_ext
        simpa [h.lengths] using hjn.symm
    · simp only [if_neg hq]
      constructor
      · intro hj
        have hkey := (h.indexed q j).mp hj
        have hb := (List.getElem?_eq_some_iff.mp hkey).1
        rw [List.getElem?_append_left hb]
        exact hkey
      · intro hj
        by_cases hb : j.val < m.keys.val.length
        · rw [List.getElem?_append_left hb] at hj
          exact (h.indexed q j).mpr hj
        · rw [List.getElem?_append_right (by omega)] at hj
          have : q = k := List.mem_singleton.mp (List.mem_of_getElem? hj)
          exact False.elim (hq this)

private theorem zip_set_right (ks : List K) (vs : List V) (i : Nat) (k : K) (v : V)
    (hk : ks[i]? = some k) :
    ks.zip (vs.set i v) = (ks.zip vs).set i (k, v) := by
  induction ks generalizing vs i with
  | nil => simp at hk
  | cons a ks ih =>
    cases vs with
    | nil => simp
    | cons b vs =>
      cases i with
      | zero => simp at hk; subst a; rfl
      | succ i =>
        simp only [List.getElem?_cons_succ] at hk
        simpa using ih vs i hk

/-- Insert preserves the representation and implements insertion-order sequence
    update. Capacity and successful key cloning are needed only for a new key. -/
theorem insert_correct (clone : core.clone.Clone K)
    (eq : core.cmp.Eq K) (hash : core.hash.Hash K)
    (m : IndexMap K V) (h : Valid m) (k : K) (v : V)
    (hkeys : LawfulKeys eq hash) (hentry : HashMap.CanEntry m.indices k)
    (hgrowth : m.indices.lookup k = none → CanPush m.keys k ∧ CanPush m.values v)
    (hclone : m.indices.lookup k = none → clone.clone k = ok k)
    (hcapacity : m.indices.lookup k = none → m.keys.val.length < Usize.max) :
    ∃ old m', IndexMap.insert clone eq hash m k v = ok (old, m') ∧ Valid m' ∧
      match m.indices.lookup k with
      | none => old = none ∧ items m' = items m ++ [(k, v)]
      | some i => old = m.values.val[i.val]? ∧ m'.keys = m.keys ∧
          items m' = (items m).set i.val (k, v) := by
  cases hi : m.indices.lookup k with
  | none =>
    have hk := hcapacity hi
    have hv : m.values.val.length < Usize.max := by have := h.lengths; omega
    let m' : IndexMap K V := ⟨m.indices.set k (Slice.len m.values),
      ⟨m.keys.val ++ [k], by simp; omega⟩,
      ⟨m.values.val ++ [v], by simp; omega⟩⟩
    refine ⟨none, m', insert_vacant clone eq hash m k v hi (hclone hi) hkeys hentry
        (hgrowth hi).1 (hgrowth hi).2 hk hv,
      valid_appended m h k v hi hk hv, ?_⟩
    simpa [hi, items, m'] using List.zip_append (r₁ := [k]) (r₂ := [v]) h.lengths
  | some i =>
    obtain ⟨old, hv⟩ := value_exists m h k i hi
    refine ⟨some old, { m with values := Slice.set m.values i v },
      insert_occupied clone eq hash m k v old i hi hv hkeys hentry,
      valid_occupied m h i v, ?_⟩
    simp only []
    refine ⟨hv.symm, trivial, ?_⟩
    exact zip_set_right m.keys.val m.values.val i.val k v ((h.indexed k i).mp hi)

end Indexmap

set_option pp.fullNames true

/-- info: 'Indexmap.new_valid' depends on axioms: [propext, hashmap.fxhash.FxHasher] -/
#guard_msgs in
#print axioms Indexmap.new_valid

/--
info: 'Indexmap.get_correct' depends on axioms: [propext,
 Classical.choice,
 Quot.sound,
 Indexmap.DependencyContracts.vec_index,
 hashmap.fxhash.FxHasher,
 CoreModels.std.collections.hash.map.LawfulKeys,
 hashmap.fxhash.FxHasher.Insts.CoreDefaultDefault.default,
 hashmap.fxhash.FxHasher.Insts.CoreHashHasher.finish,
 hashmap.fxhash.FxHasher.Insts.CoreHashHasher.write,
 CoreModels.std.collections.hash.map.HashMap.get,
 CoreModels.std.collections.hash.map.HashMap.get_identity]
-/
#guard_msgs in
#print axioms Indexmap.get_correct

/--
info: 'Indexmap.insert_correct' depends on axioms: [propext,
 Classical.choice,
 Quot.sound,
 Indexmap.DependencyContracts.CanPush,
 Indexmap.DependencyContracts.vec_index_mut,
 Indexmap.DependencyContracts.vec_push,
 hashmap.fxhash.FxHasher,
 CoreModels.std.collections.hash.map.LawfulKeys,
 hashmap.fxhash.FxHasher.Insts.CoreDefaultDefault.default,
 hashmap.fxhash.FxHasher.Insts.CoreHashHasher.finish,
 hashmap.fxhash.FxHasher.Insts.CoreHashHasher.write,
 CoreModels.std.collections.hash.map.HashMap.CanEntry]
-/
#guard_msgs in
#print axioms Indexmap.insert_correct
