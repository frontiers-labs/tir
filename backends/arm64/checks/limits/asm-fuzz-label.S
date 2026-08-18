# The fuzzer found ".0" crashed the assembly parser; it must be a clean error.
# RUN: not tir mc --march=arm64 %s
.0
