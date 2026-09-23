# Whetstone

`whetstone.c` is the unmodified Netlib C double-precision Whetstone 1.2
workload from <https://www.netlib.org/benchmark/whetstone.c>, retrieved on
2026-09-11. Its SHA-256 digest is
`333e4ceca042c146f63eec605573d16ae8b07166cbc44a17bec1ea97c6f1efbf`.

The benchmark manifest runs enough loops for the fastest reference compiler to
take several wall-clock seconds on the validation host. Run the numerical check
from the repository root:

```sh
python3 benchmarks/programs/whetstone/verify.py --fcc target/release/fcc
```

The checker builds `PRINTOUT` variants at `-O0` and `-O2`, compares all ten
reported module states with GCC and Clang, and validates its comparator with a
corrupted result. Integer fields must match exactly. Floating fields may differ
by at most one unit in the last printed decimal place, derived from each
`%12.4e` result's exponent.
