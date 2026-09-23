# Dhrystone

The sources are Reinhold P. Weicker's Dhrystone C 2.1, dated 1988-05-25,
from [Netlib's `dhry-c` archive](https://www.netlib.org/benchmark/dhry-c),
retrieved on 2026-09-12. The archive SHA-256 is
`038a7e9169787125c3451a6c941f3aca5db2d2f3863871afcdce154ef17f4e3e`.
`README_C`, `RATIONALE`, and `VARIATIONS` retain the upstream documentation
and measurement rules.

The local port adds C17 function declarations, standard library headers,
pointer-safe diagnostic printing, and an explicit successful return from
`main`. The iteration count comes from the command line instead of stdin.
The measurement loop and benchmark procedure bodies retain the upstream
operations. The manifest runs 100 million iterations with `time()` timing.
The external runner reports wall time independently of Dhrystone's timer.

Check a built executable with:

```sh
python3 benchmarks/programs/dhrystone/verify.py ./dhrystone
```

The check compares all final values with `expected.out` and requires the two
record pointers to be equal and non-null before removing their addresses from
the comparison. Timing output does not enter the comparison.
