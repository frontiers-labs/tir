// RUN: not tir mc --march=armv8-a --mattr=-advsimd %s

.global vector_add
vector_add:
    add v0, v1, v2
