// Vector arithmetic is Advanced SIMD, so it is rejected once the feature is
// disabled even though the syntax is valid.

// RUN: not tir mc --march=armv8-a --mattr=-advsimd %s

.global vector_add
vector_add:
    add v0.4s, v1.4s, v2.4s
