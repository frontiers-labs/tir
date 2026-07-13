; Instruction-selection rewrite theory. This file is parsed and checked by the
; core; target-specific bridge axioms are generated from its search section.
(theory
  (search
    (max-ops 3)
    (candidates-per-class 4)
    (operators add sub mul and or xor shl lshr ashr)
    (goal sext extension
      (leaves zero one n w w-minus-n ones-n)
      (widths (8 32) (16 32) (8 64) (16 64) (32 64)))
    (goal zext extension
      (leaves zero one n w w-minus-n ones-n)
      (widths (8 32) (16 32) (8 64) (16 64) (32 64)))
    (goal neg unary
      (leaves zero one w ones-w)
      (widths (8 8) (32 32) (64 64)))
    (goal not unary
      (leaves zero one w ones-w)
      (widths (8 8) (32 32) (64 64))))

  (family bool-materialize
    (requires if)
    (axiom eq-via-if (root 1) (lhs (eq a b))
      (rhs (if root (zext (const 1 1) (const 1 1)) (zext (const 0 1) (const 1 1)))))
    (axiom ne-via-if (root 1) (lhs (ne a b))
      (rhs (if root (zext (const 1 1) (const 1 1)) (zext (const 0 1) (const 1 1)))))
    (axiom lt-via-if (root 1) (lhs (lt a b))
      (rhs (if root (zext (const 1 1) (const 1 1)) (zext (const 0 1) (const 1 1)))))
    (axiom le-via-if (root 1) (lhs (le a b))
      (rhs (if root (zext (const 1 1) (const 1 1)) (zext (const 0 1) (const 1 1)))))
    (axiom gt-via-if (root 1) (lhs (gt a b))
      (rhs (if root (zext (const 1 1) (const 1 1)) (zext (const 0 1) (const 1 1)))))
    (axiom ge-via-if (root 1) (lhs (ge a b))
      (rhs (if root (zext (const 1 1) (const 1 1)) (zext (const 0 1) (const 1 1)))))
    (axiom ult-via-if (root 1) (lhs (ult a b))
      (rhs (if root (zext (const 1 1) (const 1 1)) (zext (const 0 1) (const 1 1)))))
    (axiom ule-via-if (root 1) (lhs (ule a b))
      (rhs (if root (zext (const 1 1) (const 1 1)) (zext (const 0 1) (const 1 1)))))
    (axiom ugt-via-if (root 1) (lhs (ugt a b))
      (rhs (if root (zext (const 1 1) (const 1 1)) (zext (const 0 1) (const 1 1)))))
    (axiom uge-via-if (root 1) (lhs (uge a b))
      (rhs (if root (zext (const 1 1) (const 1 1)) (zext (const 0 1) (const 1 1))))))

  (family comparison-materialize
    (requires if xor)
    (axiom eq-via-cmp (vars (a w) (b w)) (root 1) (lhs (eq a b)) (rhs (ult (xor a b) 1)))
    (axiom ne-via-cmp (vars (a w) (b w)) (root 1) (lhs (ne a b)) (rhs (xor (ult (xor a b) 1) (const 1 1))))
    (axiom ge-via-cmp (vars (a w) (b w)) (root 1) (lhs (ge a b)) (rhs (xor (lt a b) (const 1 1))))
    (axiom le-via-cmp (vars (a w) (b w)) (root 1) (lhs (le a b)) (rhs (xor (lt b a) (const 1 1))))
    (axiom uge-via-cmp (vars (a w) (b w)) (root 1) (lhs (uge a b)) (rhs (xor (ult a b) (const 1 1))))
    (axiom ule-via-cmp (vars (a w) (b w)) (root 1) (lhs (ule a b)) (rhs (xor (ult b a) (const 1 1)))))

  (family sub-immediate
    (requires add)
    (axiom sub-via-add-neg (vars (a w)) (consts (c w)) (root w)
      (lhs (sub a c)) (rhs (add a (neg c))))))
