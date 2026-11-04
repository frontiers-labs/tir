import Aeneas
import CoreModels

/- Trusted dependency interpretation for this proof. HashMap is observed only as
   a partial function. Rust Eq must agree with Lean equality, Clone must preserve
   keys, and Borrow must preserve the lookup relation. Buckets, hashing, allocation
   and capacity growth are outside this model. No TIR operation is defined here. -/
open Aeneas.Std

axiom hashmap.fxhash.FxHasher : Type

namespace CoreModels.core.hash

structure BuildHasherDefault (H : Type) where

structure BuildHasher (S H : Type) where
  build_hasher : S → RustM H

def BuildHasherDefault.Insts.CoreDefaultDefault (H : Type) :
    CoreModels.core.default.Default (BuildHasherDefault H) :=
  ⟨RustM.ok ⟨⟩⟩

def BuildHasherDefault.Insts.CoreHashBuildHasher {H : Type}
    (d : CoreModels.core.default.Default H) (_ : Hasher H) :
    BuildHasher (BuildHasherDefault H) H := ⟨fun _ => d.default⟩

end CoreModels.core.hash

namespace CoreModels.std.collections.hash.map

structure HashMap (K V S A : Type) where
  lookup : K → Option V

structure OccupiedEntry (K V S A : Type) where
  map : HashMap K V S A
  keyValue : K
  value : V

structure VacantEntry (K V S A : Type) where
  map : HashMap K V S A
  keyValue : K
  inserted : Option V

inductive Entry (K V S A : Type) where
  | Occupied : OccupiedEntry K V S A → Entry K V S A
  | Vacant : VacantEntry K V S A → Entry K V S A

noncomputable def HashMap.set {K V S A : Type}
    (m : HashMap K V S A) (k : K) (v : V) : HashMap K V S A := by
  classical
  exact ⟨fun q => if q = k then some v else m.lookup q⟩

end CoreModels.std.collections.hash.map
