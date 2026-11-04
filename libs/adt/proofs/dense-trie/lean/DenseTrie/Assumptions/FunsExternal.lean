-- [tir_adt]: external functions.
-- Seeded by hax from Extraction/FunsExternal_Template.lean: fill the holes.
-- hax never modifies this file; after re-extraction, compare it against the
-- regenerated template to see what changed.
import Aeneas
import CoreModels
import DenseTrie.Extraction.Types
open CoreModels Aeneas
open Aeneas.Std hiding namespace core alloc
open RustM ControlFlow Error
open Std.Do
set_option linter.dupNamespace false
set_option linter.hashCommand false
set_option linter.unusedVariables false
set_option linter.style.whitespace false
set_option linter.style.setOption false
set_option linter.style.longLine false

/- You can set the `maxHeartbeats` value with the `-max-heartbeats` CLI option -/
set_option maxHeartbeats 1000000

/- You can set the `maxRecDepth` value with the `-max-recdepth` CLI option -/
set_option maxRecDepth 2048
open tir_adt

namespace CoreModels.core.slice.Slice

/-- The standard-library operation, restricted below to pure predicates. -/
axiom partition_point {T F : Type} (inst : core.ops.function.FnMut F T Bool)
    (s : Slice T) (c : F) : RustM Usize

/-- Rust's partition-point contract for a partitioned slice and an unchanged closure. -/
axiom partition_point_split {T F : Type} (inst : core.ops.function.FnMut F T Bool)
    (s : Slice T) (c : F) (p : T → Bool) (k : Usize)
    (hpure : ∀ x ∈ s.val, inst.call_mut c x = RustM.ok (p x, c))
    (hk : k.val ≤ s.val.length)
    (hsplit : ∀ (i : Nat) (hi : i < s.val.length), p s.val[i] = decide (i < k.val)) :
    partition_point inst s c = RustM.ok k

end CoreModels.core.slice.Slice
