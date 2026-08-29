# CoreMark memory baseline

Measured with the `TIR_MEM_STATS` reporter (`fcc --mem-report`) at commit
`8e5b3699` + the instrumentation change, `cargo build --release -p fcc`, host
x86_64 Linux. Corpus: the five CoreMark benchmark translation units, compiled
one at a time, strictly serially:

```
fcc -O2 -Ilinux -Iposix -I. -DFLAGS_STR='"-O2"' -DITERATIONS=0 -c <file>
```

`-O2` is not yet honoured by the driver (it warns and ignores), so these are the
default pipeline's numbers: lower-cir → mem2reg → instcombine → scf-to-cfg →
instruction-select → register-allocation → emission. Peak RSS is the kernel's
`VmHWM`, cross-checked against `/usr/bin/time -v`.

## Per-file peak RSS

| translation unit | peak RSS | floor before first pass | `instruction-select` ΔVmHWM | share of peak |
|---|---|---|---|---|
| core_list_join.c | 295,908 kB | 15,796 kB | 275,028 kB | 93% |
| core_state.c | 202,884 kB | 15,432 kB | 182,064 kB | 90% |
| core_main.c | 63,660 kB | 16,216 kB | 42,316 kB | 66% |
| core_util.c | 24,764 kB | 14,512 kB | 4,544 kB | 18% |
| core_matrix.c | 24,728 kB | 15,204 kB | 2,820 kB | 11% |

No translation unit hit the multi-GB range the plan assumed; the worst is
289 MB. The ≤ 2 GB post-Stage-1 target is therefore already met, and the
meaningful next ratchet is the ≤ 512 MB / ≤ 256 MB pair — which the backend
alone would blow on a function only a few times larger than `core_list_join`'s
worst (the top consumer below is quadratic in a per-class quantity).

Every remaining pass is noise by comparison: the largest non-backend delta
anywhere in the corpus is `instcombine` at 1,840 kB (core_main), then `mem2reg`
at 192 kB.

## Top three consumers

### 1. isel PBQP cover matrices — up to 70.6 MB in a single solve

`backend/isel/cover.rs:build_eclass_cover` builds one dense `rows × cols`
`u64` matrix per edge, where a node's alternative count is the number of
pattern matches rooted at its e-class.

| file | largest solve | nodes | edges | bytes/edge | matrix bytes over all solves | solves |
|---|---|---|---|---|---|---|
| core_list_join.c | 70,649,984 B | 41 | 93 | 760 kB | 308,209,072 B | 100 |
| core_state.c | 37,334,928 B | 95 | 277 | 135 kB | 369,657,744 B | 146 |
| core_main.c | 5,725,784 B | 70 | 145 | 39 kB | 94,475,392 B | 112 |
| core_util.c | 426,152 B | 55 | 140 | 3 kB | 2,449,856 B | 46 |
| core_matrix.c | 67,872 B | 22 | 30 | 2 kB | 459,696 B | 58 |

760 kB per edge is ~95,000 `u64` entries, i.e. roughly 308 alternatives on each
endpoint. `pbqp::solve` additionally clones the whole problem
(`utils/pbqp/src/lib.rs:316`), so the peak is ~2× the figures above: ~141 MB
live for one 41-node cover. This single site explains 90–93% of the peak on the
two worst files.

Fixes, in the order they pay: hash-cons the matrices (Stage 6e — most edges
between two classes with the same alternative lists are the same matrix), do not
clone the problem inside `solve`, and cap or prune per-class alternatives.

### 2. regalloc PBQP matrices, rebuilt per spill round — up to 11.5 MB per solve

| file | largest solve | nodes | edges | bytes/edge | matrix bytes over all solves | solves |
|---|---|---|---|---|---|---|
| core_main.c | 11,486,120 B | 791 | 13,035 | 881 B | 16,990,968 B | 43 |
| core_list_join.c | 3,568,016 B | 166 | 2,678 | 1,332 B | 15,050,744 B | 17 |
| core_state.c | 3,021,504 B | 492 | 4,774 | 633 B | 12,872,760 B | 28 |
| core_matrix.c | 913,400 B | 131 | 1,031 | 886 B | 10,536,992 B | 23 |
| core_util.c | 226,408 B | 44 | 223 | 1,015 B | 709,680 B | 15 |

Interference matrices are structurally identical within a register class, so
hash-consing collapses the totals to a handful of distinct matrices. The solve
counts exceed the function counts because the spill-retry loop
(`backend/regalloc.rs:668`) rebuilds the entire problem each round — 43 solves
for core_main. `register-allocation` is the #2 pass on the two small files
(+1,516 kB on core_matrix, +256 kB on core_util).

### 3. Context slab churn and absent value reclamation

End-of-pipeline census (`tir-mem: context`):

| file | ops slab | ops live | slab/live | values slab | blocks slab | regions slab |
|---|---|---|---|---|---|---|
| core_main.c | 6,335 | 2,372 | 2.7× | 2,845 | 476 | 341 |
| core_state.c | 5,946 | 2,389 | 2.5× | 2,252 | 458 | 339 |
| core_list_join.c | 4,826 | 1,759 | 2.7× | 2,454 | 407 | 228 |
| core_matrix.c | 2,670 | 1,141 | 2.3× | 1,261 | 252 | 114 |
| core_util.c | 1,570 | 577 | 2.7× | 574 | 167 | 117 |

Confirms the plan's suspect 1 qualitatively — every slab is 2.3–2.7× the live
IR — but sizes it at single-digit MB (~400 B/op × 6,335 ops ≈ 2.5 MB), i.e.
~1% of the peak on the worst file. The value slab reports `slab == live`
because `Context` has no value-removal path at all: nothing is ever freed, so
the census cannot distinguish live values from leaked ones by counting. That is
itself the finding; a leak test needs reachability, not slot occupancy. Fix for
correctness of the Stage-1 ratchet, not for bytes.

## Suspects the numbers clear

- **E-graph views.** Largest anywhere: 2,761 nodes / 755,580 approx bytes
  (core_main); core_list_join peaks at 414 nodes / 108,180 B. Under 4% of peak
  RSS. The per-node budget matters for cache behaviour, not for peak memory.
  The census now sums the engine's columns — rows, children, the per-class
  arrays, the parent back-edges and the interned labels — where it used to
  charge a node struct plus a flat 64 B per class and exclude children and the
  hash-cons, so the number is larger than the one it replaces and measures more
  of the graph, not more memory.
- **TMDL sem blobs / attribute strings.** The pre-first-pass floor is
  14.5–16.2 MB and barely varies with input size, so target tables are decoded
  once and shared. Nothing here scales with the program.

## Reproducing

```sh
TIR_MEM_STATS=1 fcc -O2 -Ilinux -Iposix -I. -DFLAGS_STR='"-O2"' \
  -DITERATIONS=0 -c core_state.c -o /tmp/core_state.o 2>&1 | grep tir-mem
```

Line kinds, all on stderr: `pass` (per-pass VmHWM/VmRSS delta), `context`
(slab vs live census after each pass), `egraph` (nodes/classes/bytes per
saturation), `pbqp` (nodes/edges/matrix bytes per solve, one line per spill
round), `summary` + `top-pass` (process peak and per-pass deltas, worst first).
The reporter is inert unless enabled: compiler output is byte-identical with
and without it (verified on core_state.o).
