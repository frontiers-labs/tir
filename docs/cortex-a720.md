# Cortex-A720 scheduling model

Select the table by CPU name or explicitly:

```sh
tir sched --march=armv8.0-a --mcpu=cortex-a720 block.s
tir sched --march=armv8.0-a --model=cortex-a720 block.s
```

The table binds measured operation classes to measured resources. It omits
reorder-buffer, queue, and register-file parameters rather than borrowing values
from another model. Classes without execution-resource measurements share a
single conservative `Unmodeled` resource. That resource prevents the scheduler
from treating fallback operations as free; it does not represent an A720
hardware pipeline. Conservative fallback latencies do not undercut the closest
measured operation class.

Unmeasured SIMD operations, branches and system instructions, high multiply,
FP moves and comparisons, FP/vector loads, and stores use that conservative
route. Their fallback latency is not presented as an A720 measurement.

## Evidence

Block-level counter measurements were used only as a dispatch-width sanity
check. Full automatic inference was rejected because the difference-of-counters
method produced impossible upper-bound outliers for small deltas. The model uses
replicated single-instruction throughput and dependency recurrences instead of
corpus maxima or a fit over application blocks.

The direct measurements ran on an isolated Cortex-A720 core. Three independent
repetitions agree on the modeled integer, floating-point, vector, and load
classes. A seven-repetition cache pointer chase reports 4.000 cycles from 4
through 48 KiB before the first latency knee.

| Parameter | Model | Measurement basis |
| --- | ---: | --- |
| Dispatch width | 5 | Accepted split-specific IPC p99 values are approximately 5; impossible tails are rejected |
| Integer ALU capacity | 4/cycle | Direct add throughput 3.938/cycle |
| Integer ALU latency | 1 | Direct dependency chain |
| Integer multiply capacity | 2/cycle | Direct independent operations |
| Integer multiply latency | 2 | Direct dependency chain |
| Word divide fallback latency | 12 | Maximum observed across the operand grid |
| XLEN divide fallback latency | 20 | Maximum observed across the operand grid |
| Integer divide initiation interval | 6 | Independent one-instruction blocks |
| FP/vector capacity | 2/cycle | Direct scalar FP, vector FP, and vector-integer operations |
| FP add latency | 2 | Direct dependency chain |
| FP multiply latency | 3 | Direct dependency chain |
| FP divide latency, single/double | 8 / 13 | Operand-chain cost minus the measured restoration chain |
| FP divide initiation interval | 1 | Accepted independent one-instruction blocks |
| FP conversion latency | 3 | One-instruction dependency recurrences |
| Vector add latency | 2 | Direct FP and integer vector chains |
| L1 load latency | 4 | Direct pointer chase |
| Modeled load capacity | 2/cycle | Conservative integral approximation of stable 2.33/cycle direct throughput |

Division latency varies with operands. The available integer grid establishes
ranges but not a stable predicate that generalizes beyond sampled values, so the
static model uses the observed maxima. The FP suite lacks a corrected predicate
for individual operand classes, so it also uses one fallback per precision.

## LLVM comparison

LLVM 22.1.8 accepts `-mcpu=cortex-a720`, reports dispatch width 5, and exposes
the same resource names and results as its `neoverse-n2` selection on the checked
Arm64 inputs. LLVM is a comparison target only; none of its values are inputs to
this table.

On the representative instruction set in
`backends/arm64/checks/sched/cortex-a720-compute.S`, the tables agree on integer
multiply, FP add/multiply, vector add, conversion latency, load latency, and the
conservative integer-divide latency. Material differences are:

| Operation | Measured | TIR | LLVM 22 |
| --- | ---: | ---: | ---: |
| Word/XLEN divide initiation interval | 6 | 6 | 12 / 20 |
| FP divide latency, single/double | 8 / 13 | 8 / 13 | 10 / 15 |
| FP divide initiation interval, single/double | 1 / 1 | 1 / 1 | 10 / 15 |
| L1 load reciprocal throughput | 0.43 | 0.50 | 0.33 |

Values are cycles per instruction except capacity and dispatch width. The load
model rounds conservatively because TMDL execution-unit occupancy is integral.
