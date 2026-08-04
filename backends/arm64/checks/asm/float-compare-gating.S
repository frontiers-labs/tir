// RUN: not tir mc --march=armv8-a --mattr=-advsimd,-fp %s

.global scalar_compare
scalar_compare:
    fcmp d0, d1
