# libquantum

The sources in `src/` are libquantum 0.2.4 from the SPEC CPU2006 1.2
[public source archive](https://spec.org/sources/cpu2006/1.2/modified/462.libquantum.tar.xz).
The archive SHA-256 is
`f91fa2299a96423d0e94a15a4a062e79c451c60cc8ece538e5417f23176205e6`.
The source headers specify GPL-2.0-or-later; `COPYING` contains GPL version 2.

The benchmark factors 143 using base 5. It runs with `SPEC_CPU` and
`SPEC_CPU_NEED_COMPLEX_H`, which select the supplied POSIX complex-number
configuration. `verify.py` checks the complete output against `expected.out`.
