import Indexmap.Extraction.Types

open Aeneas.Std hiding namespace core alloc
open RustM
open CoreModels

axiom hashmap.fxhash.FxHasher.Insts.CoreDefaultDefault.default :
  RustM hashmap.fxhash.FxHasher
axiom hashmap.fxhash.FxHasher.Insts.CoreHashHasher.write :
  hashmap.fxhash.FxHasher → Slice U8 → RustM hashmap.fxhash.FxHasher
axiom hashmap.fxhash.FxHasher.Insts.CoreHashHasher.finish :
  hashmap.fxhash.FxHasher → RustM U64

namespace CoreModels.std.collections.hash.map

noncomputable section
attribute [local instance] Classical.propDecidable

variable {K V S A H Q : Type}

/-- Key equality agrees with Lean equality. Hashing is stable and compatible,
    and key/hash callbacks terminate successfully. Custom Eq quotients are not
    covered by this first proof. -/
axiom LawfulKeys (eq : core.cmp.Eq K) (hash : core.hash.Hash K) : Prop

/-- The entry operation has sufficient storage and its allocation succeeds.
    This abstracts bucket capacity, layout limits, and allocation failure. -/
axiom HashMap.CanEntry (m : HashMap K V S A) (k : K) : Prop

def HashMapKVSGlobal.Insts.CoreDefaultDefault.default (K V : Type)
    {S : Type} (_ : core.default.Default S) :
    RustM (HashMap K V S alloc.alloc.Global) := ok ⟨fun _ => none⟩

/-- `entry` preserves the map until its returned mutable borrow is written back. -/
def HashMap.entry (eq : core.cmp.Eq K) (hash : core.hash.Hash K)
    (_ : core.hash.BuildHasher S H) (m : HashMap K V S A) (k : K) :
    RustM (Entry K V S A × (Entry K V S A → HashMap K V S A)) :=
  if LawfulKeys eq hash ∧ HashMap.CanEntry m k then
  let e := match m.lookup k with
    | some v => Entry.Occupied ⟨m, k, v⟩
    | none => Entry.Vacant ⟨m, k, none⟩
  ok (e, fun e => match e with
    | .Occupied o => o.map
    | .Vacant v => match v.inserted with
      | none => v.map
      | some value => v.map.set v.keyValue value)
  else fail .panic

def OccupiedEntry.get (e : OccupiedEntry K V S A) : RustM V := ok e.value

def VacantEntry.key (e : VacantEntry K V S A) : RustM K := ok e.keyValue

def VacantEntry.insert (e : VacantEntry K V S A) (v : V) :
    RustM (V × (V → VacantEntry K V S A)) :=
  ok (v, fun v => { e with inserted := some v })

/-- General borrowed lookup remains abstract. The theorem below states the
    supported identity-Borrow contract; heterogeneous Borrow is unproved. -/
axiom HashMap.get (_ : core.cmp.Eq K) (_ : core.hash.Hash K)
    (_ : core.hash.BuildHasher S H) (_ : core.borrow.Borrow K Q)
    (_ : core.hash.Hash Q) (_ : core.cmp.Eq Q)
    (m : HashMap K V S A) (q : Q) : RustM (Option V)

axiom HashMap.get_identity (eq : core.cmp.Eq K) (hash : core.hash.Hash K)
    (build : core.hash.BuildHasher S H) (borrow : core.borrow.Borrow K K)
    (hborrow : ∀ k, borrow.borrow k = ok k)
    (hkeys : LawfulKeys eq hash)
    (m : HashMap K V S A) (k : K) :
    HashMap.get eq hash build borrow hash eq m k = ok (m.lookup k)

axiom HashMap.contains_key (_ : core.cmp.Eq K) (_ : core.hash.Hash K)
    (_ : core.hash.BuildHasher S H) (_ : core.borrow.Borrow K Q)
    (_ : core.hash.Hash Q) (_ : core.cmp.Eq Q)
    (m : HashMap K V S A) (q : Q) : RustM Bool

end
end CoreModels.std.collections.hash.map

namespace Indexmap.DependencyContracts

/-- Successful growth includes the real Vec layout and isize::MAX byte-capacity
    limits and a successful allocation. The model does not claim that a length
    bound alone makes Rust Vec::push succeed. -/
axiom CanPush {V : Type} (xs : alloc.vec.Vec V) (v : V) : Prop

/-- Standard Vec API contracts at the trusted dependency boundary. -/
axiom vec_index {V : Type} (xs : alloc.vec.Vec V) (i : Usize) (v : V)
    (h : xs.val[i.val]? = some v) :
    alloc.vec.Vec.Insts.CoreOpsIndexIndex.index
      (core.Usize.Insts.CoreSliceIndexSliceIndexSliceT V) xs i = ok v

axiom vec_index_mut {V : Type} (xs : alloc.vec.Vec V) (i : Usize) (v : V)
    (h : xs.val[i.val]? = some v) :
    alloc.vec.Vec.Insts.CoreOpsIndexIndexMut.index_mut
      (core.Usize.Insts.CoreSliceIndexSliceIndexSliceT V) xs i =
      ok (v, fun x => Slice.set xs i x)

axiom vec_push {V : Type} (xs : alloc.vec.Vec V) (v : V)
    (h : xs.val.length < Usize.max) (hgrowth : CanPush xs v) :
    alloc.vec.Vec.push xs v =
      ok ⟨xs.val ++ [v], by simp; omega⟩

end Indexmap.DependencyContracts
