; Target-independent instruction-selection invariants.
(theory
  (axiom sext-bridge (vars (x n)) (root w) (where (< n w))
    (lhs (sext x w)) (rhs (ashr (shl x (- w n)) (- w n))))

  (axiom zext-mask (vars (x n)) (root w) (where (< n w))
    (lhs (zext x w)) (rhs (and (ones n) x)))
  (axiom zext-shifts (vars (x n)) (root w) (where (< n w))
    (lhs (zext x w)) (rhs (lshr (shl x (- w n)) (- w n))))


  (axiom neg-sub (vars (x w)) (root w) (lhs (neg x)) (rhs (sub 0 x)))
  (axiom neg-mul (vars (x w)) (root w) (lhs (neg x)) (rhs (mul (ones w) x)))

  (axiom not-sub (vars (x w)) (root w) (lhs (not x)) (rhs (sub (ones w) x)))
  (axiom not-sub-one (vars (x w)) (root w) (lhs (not x)) (rhs (sub (sub 0 x) 1)))
  (axiom not-xor (vars (x w)) (root w) (lhs (not x)) (rhs (xor (ones w) x)))
  (axiom not-add-sub (vars (x w)) (root w) (lhs (not x)) (rhs (sub 0 (add 1 x))))

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
    (rhs (if root (zext (const 1 1) (const 1 1)) (zext (const 0 1) (const 1 1)))))

  (axiom eq-via-cmp (vars (a w) (b w)) (root 1) (lhs (eq a b)) (rhs (ult (xor a b) 1)))
  (axiom ne-via-cmp (vars (a w) (b w)) (root 1) (lhs (ne a b)) (rhs (xor (ult (xor a b) 1) (const 1 1))))
  (axiom ge-via-cmp (vars (a w) (b w)) (root 1) (lhs (ge a b)) (rhs (xor (lt a b) (const 1 1))))
  (axiom le-via-cmp (vars (a w) (b w)) (root 1) (lhs (le a b)) (rhs (xor (lt b a) (const 1 1))))
  (axiom uge-via-cmp (vars (a w) (b w)) (root 1) (lhs (uge a b)) (rhs (xor (ult a b) (const 1 1))))
  (axiom ule-via-cmp (vars (a w) (b w)) (root 1) (lhs (ule a b)) (rhs (xor (ult b a) (const 1 1))))

  (axiom sub-via-add-neg (vars (a w)) (consts (c w)) (root w)
    (lhs (sub a c)) (rhs (add a (neg c)))))
