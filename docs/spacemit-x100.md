# SpacemiT X100 scalar scheduling model

Use the X100 table with either CPU selection or an explicit model:

```sh
tir sched --march=rv64imfdc --mcpu=spacemit-x100 block.s
tir sched --march=rv64imfdc --model=spacemit-x100 block.s
```

The table describes scalar execution. `--march` still controls instruction
availability. Selecting X100 does not enable the entire RVA23 profile or supply
vector timing dependent on VL, SEW, and LMUL.

## Evidence and assumptions

The model combines the X100 architecture description with isolated constraints
from the inference split of `uarch/dataset-rva23`. It does not search for constants
that minimize errors on application blocks. Selection and holdout projects are
kept separate from inference, including the corpus's exact-body deduplication.

The [SpacemiT K3 architecture paper](https://forum.spacemit.com/uploads/short-url/60aJ8cYNmrFWqHn4ddwwSzMLjlY.pdf),
section 3.1.2, describes four-wide decode/dispatch and three integer ALUs. The
third ALU shares register read ports with the branch unit. Accordingly, ordinary
integer instructions can use any of three routes, while branches contend with
the third route. This differs from treating LLVM's queue acceptance capacities
as execution-unit counts.

The [SpacemiT-contributed LLVM table](https://github.com/llvm/llvm-project/blob/a67c8f208e87/llvm/lib/Target/RISCV/RISCVSchedSpacemitX100.td)
supplies priors where the corpus cannot isolate a parameter. These are not
independent measurements by TIR.

| Parameter | X100 model | Basis |
| --- | --- | --- |
| Dispatch width | 4 | Vendor architecture |
| Integer capacity | 3 instructions/cycle | Vendor topology; inference ALU floor near 0.34 cycles/instruction |
| Integer latency | 1 cycle | Single-instruction dependency recurrences |
| AUIPC capacity | 1 instruction/cycle | 13,911 single-instruction inference blocks, median 1.013 cycles |
| Multiply capacity | 1 instruction/cycle | 109 independent single-instruction inference blocks, median 1.001 cycles |
| Multiply latency | 2 cycles for `mulw`, 3 for XLEN multiply | Recurrences and LLVM prior |
| FP arithmetic capacity | 2 instructions/cycle | 1,977 independent single-instruction inference blocks, median 0.505 cycles |
| FP add, sign, min/max, compare, moves | 3 cycles | LLVM prior; add recurrence evidence |
| FP multiply | 4 cycles | LLVM prior |
| FP fused multiply-add | 5 cycles for single, 4 for double | LLVM prior |
| FP conversion | 3 cycles on one FP route | LLVM prior |
| Integer divide/remainder | 14 cycles for word, 22 for XLEN | Conservative LLVM bounds |
| FP divide/sqrt fallback | 15 cycles for single, 23 for double | Controlled subnormal divide chains; conservative shared fallback |
| Load latency | 3 cycles integer, 4 FP | LLVM L1-hit priors, not measured by this corpus |
| Load/store capacity | 2 instructions/cycle | Vendor memory-pipeline description |
| Branch latency | 2 cycles | LLVM prior; no prediction model |
| Instruction window | At most 192 instructions | LLVM's 64 ROB entries with up to 3 micro-ops per entry |

The instruction window is an upper-bound approximation. TIR does not reproduce
ROB packing, per-queue admission limits, or the physical register-file capacities
of X100. No invented queue or register-file size is fitted to compensate.

Static division costs retain conservative fallback estimates. Repeated benchmark bodies
can converge to zero, one, or other operands that take a short path. A low
aggregate error obtained by replacing the divider's bound with those short-path
costs would not establish a generally accurate divider model. Divider occupancy
uses the same fallback estimate, with separate integer and FP divider resources.
Dynamic execution can select shorter measured paths as described below.

## Measurement protocol and limits

The current measurement inventory contains 1,100,000 rows in 110 Parquet files.
729,412 rows are accepted, 370,210 are unstable, and 378 report counter failures.
The analysis uses accepted rows with finite positive cycle counts, matching
instruction counts, and 64 iterations of the unrolled body. It excludes
`measurements-invalid-singlepass-s00`, whose cold instruction fetch dominated
the measurements. The dataset's `MEASURE.md` status section predates the current
11-shard inventory.

The compute corpus excludes memory accesses, barriers, traps, and writes to the
harness stack pointer. It therefore cannot validate memory timing, caches,
atomics, or control-flow prediction. Operand-dependent execution remains a
source of error. Vector blocks are excluded from the scalar comparison, and unsupported
scalar instructions are reported as coverage failures.

The initial comparison uses the lowest 300 canonical fingerprints per project
after fixed quality checks. Selection contains Bash and CPython; holdout contains
OpenBLAS and SQLite. This is a deterministic sample, not an outcome-dependent
choice of easy blocks. It represents two held-out projects, not all RISC-V code.

Both tools receive assembly disassembled from the original instruction bytes.
Their predicted cycles per block are the difference between 200-iteration and
100-iteration runs divided by 100, removing fixed startup/drain costs. A pair is
scored only when both tools accept the complete block and report the expected
instruction count. Failures and exclusions remain in the coverage report.

## Initial frozen comparison

The comparison uses LLVM 24.0.0git at
`a67c8f208e8710ad8b6b6e2ca4de57e4e0db7178`, built with the RISC-V target.
The installed LLVM 22.1.8 does not recognize `spacemit-x100`; it is not used as
the timing baseline. The X100 table's SHA-256 at freeze is
`b7f9129e64bd3ebae08a83dcf46890e886288fb016db1e8135e461d31317b3c2`.

| Split | Paired blocks | TIR MAE, cycles/block | LLVM MAE, cycles/block | TIR median relative error | LLVM median relative error |
| --- | ---: | ---: | ---: | ---: | ---: |
| Selection | 533 | 0.0584 | 0.2868 | 1.21% | 25.96% |
| Holdout | 522 | 0.5080 | 0.4630 | 1.11% | 25.78% |

Each split sampled 600 blocks. Selection excluded 14 vector blocks and 53
unsupported or unparseable scalar blocks. Holdout excluded 9 vector blocks and
69 unsupported or unparseable scalar blocks. All paired TIR inputs were
re-encoded and matched the original bytes. These exclusions are coverage limits,
not accurate predictions.

On holdout, TIR places 88.51% of paired blocks within 10% of the measurement;
LLVM places 15.90% within 10%. Their 90th-percentile relative errors are 13.51%
and 38.61%. TIR nevertheless has the worse overall MAE because of large tail
errors. The project breakdown makes that distinction visible:

| Holdout project | Paired blocks | TIR MAE | LLVM MAE |
| --- | ---: | ---: | ---: |
| OpenBLAS | 243 | 1.0421 | 0.7439 |
| SQLite | 279 | 0.0429 | 0.2184 |

The 477 integer-only holdout blocks have MAE 0.1591 for TIR and 0.3443 for LLVM.
The 45 blocks containing scalar FP have MAE 4.2072 and 1.7211 respectively.
The 11 blocks containing divide or square root have MAE 17.1011 and 8.9193.
The FP and divide/sqrt categories overlap; neither was excluded from the main
result or used to retune the frozen constants.

## Comparison after the FP dependency fix

At this checkpoint the X100 table constants were unchanged. The original samples are regression
checks because their outcomes were already inspected. A fresh sample uses
canonical fingerprint ranks 301 through 600 per holdout project, selected by the
same fixed quality rules. It has no exact-body overlap with the original sample;
it still covers the same two projects and is not independent project validation.

| Sample | Paired blocks | TIR MAE | LLVM MAE | TIR within 10% | LLVM within 10% |
| --- | ---: | ---: | ---: | ---: | ---: |
| Original selection | 533 | 0.0557 | 0.2868 | 96.44% | 12.01% |
| Original holdout, regression | 522 | 0.3585 | 0.4630 | 91.00% | 15.90% |
| Fresh holdout blocks | 512 | 0.4735 | 0.4736 | 94.53% | 19.14% |

The fresh sample excludes 7 vector blocks and 81 unsupported scalar blocks.
Its median relative error is 1.01% for TIR and 25.68% for LLVM. Overall MAE is
essentially tied. FP tails remain worse for TIR: the 39 FP blocks have MAE
5.2920 versus 2.3731, and the overlapping 10 divide/sqrt blocks have MAE
22.2255 versus 9.3742. The 473 integer blocks have MAE 0.0762 versus 0.3169.
These results supported the dependency fix. Divider calibration was still
unresolved at that checkpoint. No constants were retuned from this sample.

That comparison executable, source snapshot, commands, and per-block results
are preserved under the evidence bundle's `revision2/` directory.

## Final comparison after divider calibration

The final executable uses the measured conditional cases and the larger 15/23
FP fallback. A new sample uses fingerprint ranks 601–900 per project, with zero
exact-body overlap against both earlier ranges. No constants were changed after
scoring it. These are still the same two projects, not independent project validation.

| Sample | Paired blocks | TIR MAE | LLVM MAE | TIR within 10% | LLVM within 10% |
| --- | ---: | ---: | ---: | ---: | ---: |
| Original holdout, regression | 522 | 0.4159 | 0.4630 | 91.00% | 15.90% |
| New holdout blocks, ranks 601–900 | 516 | 0.9348 | 0.5976 | 89.53% | 17.64% |

The new sample excludes 4 vector blocks and 80 unsupported or undecodable blocks.
Median relative error is 1.03% for TIR and 25.77% for LLVM. The 461 integer blocks
have MAE 0.1990 versus 0.3948; the 55 FP blocks have MAE 7.1022 versus 2.2968.
The overlapping 19 divide/sqrt blocks have MAE 24.2102 versus 8.7786.

Divider-heavy static blocks are not a merge gate. The runtime scheduling
contract is checked by the `spacemit-x100-*` LIT tests under
`simulator/isasim/checks/riscv/exec/`, which exercise `when` latency and `uop`
occupancy with known operands.

Static scheduling cannot select the measured short paths without operand state.
Its conservative fallback overestimates many repeated divider bodies, and this
sample's FP tail makes overall MAE worse than LLVM. Dynamic simulator tests
verify conditional latency and occupancy separately. Those tests establish the
implementation contract; they do not turn the static corpus comparison into a
validation of every runtime operand case.

The final source, executable, counter measurements, comparison scripts and
results are preserved in `revision3/`. Validation passed 188 distinct LIT tests,
106 symbolic unit tests, 86 simulator unit tests, formatting, generated-source
consistency, and Clippy with warnings denied for all affected packages.

### FP flag dependencies

TMDL derives scheduling-only metadata for recognized OR accumulations of a
register marked `fp_flags`, provided an operand belongs to a register class
marked `float`. CSR-only flag updates retain ordinary read/write dependencies.
Architectural reads, writes, and execution semantics
remain unchanged. Out-of-order scheduling allows independent contributions to
execute in parallel. An explicit flag reader waits for all preceding contributors;
an overwrite starts a new flag version. Unrecognized behavior retains ordinary
read/write dependencies. In-order scheduling remains conservative.

Four independent `fadd.s` instructions at 100 iterations now take 203 cycles.
A real dependency chain of the same length still takes 1,201 cycles. Tests also
cover a flag read after mixed-latency contributors, overwrite, ordinary
read/modify/write, and register aliases.

### Operand-dependent timing

TMDL `when` cases select result latency and optional resource occupancy from an
instruction's entry operands. The executor records the complete scheduling class
for timing replay. Cases without resource statements inherit fallback resources.
Static `tir sched` has no operand values and uses the fallback.

Controlled runs on X100 CPU 4 used an isolated CPU partition, movable IRQs and other
system workloads moved off CPUs 0–7, and a fixed 2.2 GHz frequency. The runner
saved the original settings and asserted their restoration after each campaign,
including removal of newly enabled cpuset controllers. Sources, raw counters,
exact operand/result bits, host
records, and verification scripts are preserved in `revision3/calibration/`.

Integer division and remainder were probed at both effective widths with a
16-by-16 operand grid, three repetitions, and 512/1,024 iterations of 128
operations. A C oracle checked every native instruction result and every timed
kernel's output. Independent operations measure divider acceptance; fixed-point
chains establish latency where the result preserves the input. Restored chains
add one dependent XOR per operation and corroborate those classes. The control's
0.5-cycle throughput is not subtracted as XOR latency. Seventeen restored-chain
rows undercounted the instruction body and were rejected. The 6,712 paired cases
differ by at most 0.012 cycles per operation between campaign medians.

All eight integer divide/remainder instructions select latency 5 and occupancy 4
for divisor zero, or latency 4 and occupancy 3 for divisor one. Word operations
compare the low 32 bits of the divisor. Other operands retain the 14/22-cycle
fallback; the small grid does not establish a general quotient-length formula.

The FP grid contains 32 signed IEEE values per precision: zeros, subnormals,
normal values near exponent and fraction boundaries, infinities, and NaNs. It
covers all input pairs for division and each input for square root. Independent
operations measure initiation interval. Output-preserving division-by-one chains
and square-root fixed points measure result latency. Reciprocal chains alternate
inputs and are excluded from single-input latency claims.

Two campaigns run 2,048 and 4,096 iterations of 128 unrolled operations, with
three repetitions per case. Nineteen rows undercounted the known instruction body
and were rejected. The remaining 2,222 paired cases differ by at most 0.0034
cycles per operation in their median rates. All 13,128 non-reciprocal result rows
match an independent host arithmetic oracle; NaN payloads are excluded from that
comparison. The initial 280-row fixed-point probe provides seven repetitions at
1,048,576 operations per sample.

| FP operand class | Result latency, single / double | Initiation interval, single / double |
| --- | --- | --- |
| Zero or nonfinite division operand; zero/nonfinite/negative sqrt input | 6 / 6 | 5 / 5 |
| Normal division with exponent difference strictly inside result boundaries; positive normal sqrt | 11 / 19 | 10 / 18 |
| Fallback, including subnormals and boundary results | 15 / 23 | 15 / 23 |

The normal division predicate requires two normal operands and an unbiased
exponent difference in `[-125, 127]` for single precision or `[-1021, 1023]` for
double. This deliberately leaves boundary cases on the fallback. The measured
subnormal numerator divided by one takes 15/23 cycles in both shapes. Positive
subnormal square root accepts operations every 13/21 cycles, but its individual
latency is not established; it also keeps the fallback.

These are scheduling estimates, not exhaustive hardware bounds. Direct latency
measurements cover fixed points; applying those latency classes to other inputs
with the same measured initiation interval is an inference. Measurements use
round-to-nearest-even. Other rounding modes share these scheduling estimates and
are not separately calibrated. The fallback is supported by the slowest measured
division class, not a proof over all operands or rounding modes.

### Copy behavior

Independent compressed copies sustain about three instructions per cycle, but
that does not determine their latency. Self-copies measure one cycle, and actual
copy rings show sequence-dependent behavior. The table retains ordinary
one-cycle ALU copies. It does not infer universal move elimination from the mere
presence of a loop-carried edge, or assign a fractional latency to fit copy rings.

### Reproduction artifacts

The accompanying evidence bundle is maintained outside the repository. It
contains the extraction/scoring
script, selected bytes and measurements, per-block predictions, exclusions,
source hashes, inference diagnostics, and LLVM build instructions. Run with
Python packages `pyarrow`, `zstandard`, and `numpy` installed:

```sh
python x100_compare.py --dataset /path/to/dataset-rva23 --split selection --out results
python x100_compare.py --dataset /path/to/dataset-rva23 --split holdout --out results
python x100_compare.py --score results/samples-selection.jsonl --out results \
  --tir /path/to/tir --llvm-mca /path/to/x100-llvm-mca
python x100_compare.py --score results/samples-holdout.jsonl --out results \
  --tir /path/to/tir --llvm-mca /path/to/x100-llvm-mca
```

Accepted historical rows all identify X100 CPUs 4 through 7, report host
readiness, use the performance governor fixed at 2.2 GHz, and run 64 body
iterations. A live host check on 2026-09-16 found no benchmark isolation
and an ondemand governor. The strict runner rejected the environment. That initial comparison used historical samples. The later controlled divider
campaigns described above supply new hardware evidence and restore all temporary
host configuration changes.
