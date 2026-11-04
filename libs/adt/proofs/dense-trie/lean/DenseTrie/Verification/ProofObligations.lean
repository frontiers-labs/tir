import DenseTrie.Extraction
import DenseTrie.Verification.Partitions

open Aeneas.Std hiding namespace core alloc
open RustM CoreModels

namespace DenseTrie

private def segment {T : Type} (s : Slice T) (first last : Nat) : Slice T :=
  ⟨s.val.slice first last, by
    have := s.val.slice_length_le first last
    have := s.property
    omega⟩

private theorem index_range {T : Type} (s : Slice T) (first last : Usize)
    (horder : first.val ≤ last.val) (hbound : last.val ≤ s.val.length) :
    core.Slice.Insts.CoreOpsIndexIndex.index
      (core.ops.range.RangeUsize.Insts.CoreSliceIndexSliceIndexSliceSlice T)
      s ⟨first, last⟩ = ok (segment s first.val last.val) := by
  simp [core.Slice.Insts.CoreOpsIndexIndex.index,
    core.ops.range.RangeUsize.Insts.CoreSliceIndexSliceIndexSliceSlice.get,
    rust_primitives.slice.slice_length, rust_primitives.slice.slice_slice,
    Slice.subslice, Slice.len, Slice.length, horder, hbound, segment]

private theorem index_from {T : Type} (s : Slice T) (first : Usize)
    (hbound : first.val ≤ s.val.length) :
    core.Slice.Insts.CoreOpsIndexIndex.index
      (core.ops.range.RangeFromUsize.Insts.CoreSliceIndexSliceIndexSliceSlice T)
      s ⟨first⟩ = ok (segment s first.val s.val.length) := by
  simp [core.Slice.Insts.CoreOpsIndexIndex.index,
    core.ops.range.RangeFromUsize.Insts.CoreSliceIndexSliceIndexSliceSlice.get,
    rust_primitives.slice.slice_length, rust_primitives.slice.slice_slice,
    Slice.subslice, Slice.len, Slice.length, hbound, segment]

private theorem add_eq {ty : UScalarTy} (a b c : UScalar ty)
    (h : a.val + b.val = c.val) : a + b = ok c := by
  have hs := UScalar.add_equiv a b
  cases heq : a + b with
  | ok out =>
    simp only [heq] at hs
    have hout : out = c := UScalar.eq_of_val_eq (hs.2.1.trans h)
    exact congrArg ok hout
  | fail error =>
    simp only [heq] at hs
    exact False.elim (hs.2 (h ▸ c.hBounds))
  | div => simp [heq] at hs

private theorem search_partitioned
    (cells : Slice U32) (range : core.ops.range.Range U32) (value : U32)
    (horder : range.start.val ≤ range.end.val)
    (hbound : range.end.val ≤ cells.val.length)
    (low high : Nat) (hlh : low ≤ high)
    (hh : high ≤ range.end.val - range.start.val)
    (hlo : ∀ (i : Nat) (hi : i < (segment cells range.start.val range.end.val).val.length),
      (segment cells range.start.val range.end.val).val[i] < value ↔ i < low)
    (heq : ∀ (i : Nat) (hi : i < (segment cells range.start.val range.end.val).val.length),
      (segment cells range.start.val range.end.val).val[i] = value ↔ low ≤ i ∧ i < high) :
    ∃ found, tir_adt.dense_trie.search cells range value = ok found ∧
      found.start.val = range.start.val + low ∧
      found.end.val = range.start.val + high := by
  let part := segment cells range.start.val range.end.val
  have hlen : part.val.length = range.end.val - range.start.val := by
    simp [part, segment, List.slice, Nat.min_eq_left (by omega :
      range.end.val - range.start.val ≤ cells.val.length - range.start.val)]
  let lo : Usize := Usize.ofNatCore low (by
    have := cells.property
    have := range.end.hBounds
    scalar_tac)
  let rest := segment part lo.val part.val.length
  have hlov : lo.val = low := by simp [lo]
  have hrest : rest.val.length = part.val.length - low := by
    simp [rest, segment, List.slice, hlov]
  let count : Usize := Usize.ofNatCore (high - low) (by
    have := cells.property
    scalar_tac)
  have hcount : count.val = high - low := by simp [count]
  have hlo_result : core.slice.Slice.partition_point
      tir_adt.dense_trie.search.closure.Insts.CoreOpsFunctionFnMutTupleSharedU32Bool
      part value = ok lo := by
    apply core.slice.Slice.partition_point_split _ _ _ (fun x => decide (x < value)) lo
    · intro x hx
      rfl
    · omega
    · intro i hi
      simp only [decide_eq_decide, hlov]
      exact hlo i hi
  have heq_result : core.slice.Slice.partition_point
      tir_adt.dense_trie.search.closure_1.Insts.CoreOpsFunctionFnMutTupleSharedU32Bool
      rest value = ok count := by
    apply core.slice.Slice.partition_point_split _ _ _ (fun x => decide (x = value)) count
    · intro x hx
      rfl
    · omega
    · intro i hi
      have hidx : i + low < part.val.length := by omega
      have hcell : rest.val[i] = part.val[low + i] := by
        exact List.getElem_slice _ _ _ _ (by omega)
      rw [hcell]
      simp only [decide_eq_decide]
      rw [heq (low + i) (by change low + i < part.val.length; omega)]
      omega
  let upper : Usize := Usize.ofNatCore high (by have := cells.property; scalar_tac)
  have hupper : upper.val = high := by simp [upper]
  have hsum : lo + count = ok upper := add_eq _ _ _ (by omega)
  let outlow : U32 := U32.ofNatCore (range.start.val + low) (by
    have := range.end.hBounds
    scalar_tac)
  let outhigh : U32 := U32.ofNatCore (range.start.val + high) (by
    have := range.end.hBounds
    scalar_tac)
  have hlowcast : (UScalar.cast .U32 lo).val = low := by
    rw [UScalar.cast_val_eq, hlov]
    exact Nat.mod_eq_of_lt (by have := range.end.hBounds; scalar_tac)
  have hhighcast : (UScalar.cast .U32 upper).val = high := by
    rw [UScalar.cast_val_eq, hupper]
    exact Nat.mod_eq_of_lt (by have := range.end.hBounds; scalar_tac)
  have haddlow : range.start + UScalar.cast .U32 lo = ok outlow :=
    add_eq _ _ _ (by simp [outlow, hlowcast])
  have haddhigh : range.start + UScalar.cast .U32 upper = ok outhigh :=
    add_eq _ _ _ (by simp [outhigh, hhighcast])
  refine ⟨⟨outlow, outhigh⟩, ?_, by simp [outlow], by simp [outhigh]⟩
  simp only [tir_adt.dense_trie.search, lift, Bind.bind, Aeneas.Std.bind_ok]
  rw [index_range cells _ _ (by simpa) (by simpa)]
  simp only [Aeneas.Std.bind_ok, U32.cast_Usize_val_eq]
  rw [hlo_result]
  simp only [Aeneas.Std.bind_ok]
  rw [index_from part lo (by omega)]
  simp only [Aeneas.Std.bind_ok]
  rw [heq_result]
  simp only [Aeneas.Std.bind_ok, hsum, haddlow, haddhigh]

/-- On any valid sorted subrange, search succeeds and returns exactly the
    matching indices. No proof-specific length bound is imposed. -/
theorem search_correct
    (cells : Slice U32) (range : core.ops.range.Range U32) (value : U32)
    (horder : range.start.val ≤ range.end.val)
    (hbound : range.end.val ≤ cells.val.length)
    (hsorted : (cells.val.slice range.start.val range.end.val).Pairwise (· ≤ ·)) :
    ∃ found, tir_adt.dense_trie.search cells range value = ok found ∧
      range.start.val ≤ found.start.val ∧ found.start.val ≤ found.end.val ∧
      found.end.val ≤ range.end.val ∧
      ∀ (i : Nat) (hi : i < cells.val.length),
        (range.start.val ≤ i ∧ i < range.end.val ∧ cells.val[i] = value) ↔
          found.start.val ≤ i ∧ i < found.end.val := by
  let part := segment cells range.start.val range.end.val
  have hlen : part.val.length = range.end.val - range.start.val := by
    simp [part, segment, List.slice, Nat.min_eq_left (by omega :
      range.end.val - range.start.val ≤ cells.val.length - range.start.val)]
  obtain ⟨low, high, hlh, hh, hlo, _, heq⟩ :=
    DenseTrieMath.sorted_partitions part.val hsorted value
  obtain ⟨found, hsearch, hstart, hend⟩ :=
    search_partitioned cells range value horder hbound low high hlh (by omega) hlo heq
  refine ⟨found, hsearch, by omega, by omega, by omega, ?_⟩
  intro i hi
  by_cases hin : range.start.val ≤ i ∧ i < range.end.val
  · have hlocal : i - range.start.val < part.val.length := by omega
    have hcell : part.val[i - range.start.val] = cells.val[i] := by
      have hs := List.getElem_slice range.start.val range.end.val
        (i - range.start.val) cells.val (by omega)
      simpa [part, segment, Nat.add_sub_of_le hin.1] using hs
    have hc := heq (i - range.start.val) hlocal
    rw [hcell] at hc
    constructor
    · rintro ⟨_, _, hequal⟩
      have := hc.mp hequal
      omega
    · intro hfound
      refine ⟨hin.1, hin.2, hc.mpr ?_⟩
      omega
  · constructor
    · rintro ⟨hs, he, _⟩
      exact False.elim (hin ⟨hs, he⟩)
    · intro hf
      exact False.elim (hin (by omega))

end DenseTrie

set_option pp.fullNames true

/--
info: 'DenseTrie.search_correct' depends on axioms: [propext,
 Classical.choice,
 Quot.sound,
 CoreModels.core.slice.Slice.partition_point,
 CoreModels.core.slice.Slice.partition_point_split]
-/
#guard_msgs in
#print axioms DenseTrie.search_correct
