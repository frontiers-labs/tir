# Modelling performance with TMDL

TIR's performance model has two halves that meet at a single source of truth:

- **What instructions exist and what they nominally cost** — declared per ISA with
  `sched_class` identities and instruction `schedule` membership.
- **How a particular device executes them** — declared per device with a `machine`
  block (issue width, pipeline, functional units, register files, latency bindings,
  forwarding).

The TMDL compiler resolves the two into one `MachineModel` per machine. Both the
compiler's cost model and the static analyzer `tir sched` (an `llvm-mca`-style
throughput tool) consume that same `MachineModel`, so they can never disagree about
an instruction's cost.

## Defining a machine

Modern SoCs include processing units of different architecture — general-purpose
CPU cores, GPUs, NPUs, DSPs, MCUs. A *machine* in TIR is any one such unit. A
machine definition binds machine-independent scheduling classes to concrete cost
for a set of ISAs:

```
machine GenericInOrder ("generic-in-order") for [RV64I] {
  issue_width = 1;

  unit ALU { count = 1; }
  unit LSU { count = 1; }

  bind WriteIALU { reads = ID; writes = EX;  uses = [ALU]; }
  bind WriteLoad { reads = ID; writes = MEM; uses = [LSU]; }

  // Bypass network: an ALU result feeds a dependent ALU op back-to-back (0-cycle
  // forward); a load result reaches a dependent ALU op with a 1-cycle bubble.
  forward ALU => ALU { latency = 0; }
  forward LSU => ALU { latency = 1; }
}
```

The optional `("alias")` after the name is the friendly name tools select with
(`--model generic-in-order`); the machine is also selectable by its declared name.

## Machines, ISAs, and `march`

A machine is declared `for [ISA, ...]`. The selected `march` — not the machine —
decides **which instructions exist**: it fixes the ISA and therefore the universe
of mnemonics, which are unique across that ISA. A machine is a *timing preference*
layered on top: it says how the chosen silicon executes those instructions.

Because the two are chosen independently, a machine may be asked to schedule an
instruction it never bound (for example, `march` pulls in an extension the machine
predates). Cost is then resolved through a **fallback chain**, most specific first:

1. a per-instruction `override` on the machine (the LLVM `InstRW` analogue);
2. otherwise, the machine's `bind` of a `sched_class` the instruction belongs to;
3. otherwise, that `sched_class`'s resource-agnostic default latency/throughput;
4. otherwise, the built-in **default**: 1-cycle latency, 1-cycle reciprocal
   throughput, fully pipelined, occupying no functional unit.

So selecting no machine at all (or a machine that covers nothing) does not fail —
every instruction simply costs one cycle. `tir sched` with no `--model` makes this
explicit: it analyzes against a generic single-issue core where the default applies
to everything.

## Issue width

`issue_width = N` is the number of instructions the front end can dispatch per
cycle. It is a general property of *any* machine, not just out-of-order ones:
modern in-order cores are frequently superscalar (e.g. dual-issue MCUs and
application cores). On an in-order machine, up to `N` instructions issue per cycle
**in program order** and only while their operands and resources are ready; on an
out-of-order machine the `N` may be drawn from anywhere in the instruction window.
The default is 1.

## Pipelines

By default a machine uses the classic 5-stage pipeline:

- `IF` — instruction fetch (read from memory/cache).
- `ID` — instruction decode (ISA instruction → micro-ops).
- `EX` — execute (the actual computation).
- `MEM` — memory access (loads/stores).
- `WB` — write-back (retire, commit results to the register file).

A machine may instead declare its own stages, in order. A stage's **position is its
cycle offset from issue**, so the pipeline list is what gives phase-based latencies
(below) their meaning:

```
// A 3-stage in-order MCU.
pipeline {
  IF;
  ID;
  EX;
}
```

Machines with fewer stages merge the trailing work into the last declared stage
(a 3-stage core does memory and write-back within `EX`). At the limit, a 1-stage
machine is effectively combinational: there are no later phases to read/write
across, so such a machine expresses cost with **scalar** `latency = N` bindings
rather than phase-based `reads`/`writes`.

Each stage carries a hazard-handling mode that tells the model whether a latency
must be honored by stalling or is exposed to the compiler:

```
pipeline {
  IF;
  ID;
  EX: protected;     // hardware interlock: the model stalls to honor latency
  MEM: unprotected;  // no interlock: the compiler scheduled around it; no stall
  WB: hard;          // fixed timing that cannot be stalled (e.g. a delay slot)
}
```

`protected` is the default and the norm for superscalar CPUs. `unprotected` models
exposed pipelines (typical of DSPs/VLIW, see below). `hard` models side-effect
timing that is fixed by the architecture and cannot be interlocked.

## Functional units and buffers

`unit NAME { count = N; }` declares a functional unit and how many parallel copies
of it exist. Bindings reserve units; the analyzer models contention against the
`count` and the instruction's reciprocal throughput.

```
unit ALU { count = 2; }
unit LSU { count = 1; }
```

`buffers { ... }` declares structural queue sizes — these are *defaults*; a
simulator may override them per experiment. A machine that declares a reorder
buffer (`rob`) is treated as out-of-order with that window; a machine with none is
in-order.

```
buffers {
  rob = 128;   // reorder buffer (presence ⇒ out-of-order, with this window)
  lsq = 32;    // load/store queue
  iq  = 64;    // issue queue
}
```

### Resource groups and micro-ops

`group` declares a reusable expression over existing units. It never creates
capacity. `|` selects one available route and `&` reserves every operand of the
route. A postfix `<N>` keeps that resource occupied for `N` cycles.

```
unit P0 { count = 1; }
unit P1 { count = 1; }
unit Multiply { count = 1; }

group Int = P0 | P1;

bind WriteIALU {
  latency = 1;
  uop(Int);
}

bind WriteIMul {
  latency = 3;
  uop(Int & Multiply<3>);
}
```

Multiple `uop(...)` statements emit multiple micro-ops. The equivalent repeated
form is `uop(Int, count = 2)`. Groups may overlap freely; contention always occurs
on their underlying `unit` declarations. The older `uses = [ALU, ...]` form remains
available and means one legacy reservation that requires every listed resource.

## Fetch and decode frontend

An out-of-order machine may describe the byte fetch path, decoder topology, and
decoded-instruction cache directly:

```
frontend {
  fetch {
    bytes_per_cycle = 16;
    window_bytes = 32;
    alignment = 16;
    queue_bytes = 64;
  }
  decode {
    slots = [complex, simple, simple, simple];
    uops_per_cycle = 6;
    queue_uops = 64;
    decoder simple { max_uops_per_instruction = 1; }
    decoder complex { max_uops_per_instruction = 4; }
  }
  decoded_cache {
    sets = 32;
    ways = 8;
    line_bytes = 32;
    line_uops = 6;
    deliver_uops_per_cycle = 6;
  }
}
```

The instruction encoder supplies exact PCs and byte lengths to `tir sched`.
Bindings normally derive their decoded micro-op count from their `uop` statements.
Exceptional classes may override it and require a particular decoder kind:

```
bind WriteComplex {
  decode_uops = 3;
  decoder = complex;
  uop(P0);
}
```

The semantic checker rejects missing or non-positive capacities, unknown decoder
slots, decoder requirements with no capable slot, unknown resources, cyclic groups,
and non-positive micro-op counts or occupancies.

## Rename-stage behavior

Cores that rename registers resolve some instructions before execution, so their
measured cost is bounded by rename/issue width rather than by any execution unit.
Two optional bind fields model this:

```
bind WriteIMove {
  latency = 0;
  eliminated = true;
}

bind WriteIXor {
  latency = 1;
  uop(Int);
  zero_idiom = true;
}
```

`eliminated = true` marks a class the rename stage completes: the instruction
reserves no execution resource and its result is available the cycle it issues,
while still consuming front-end bandwidth, issue width and a reorder-buffer slot.
It still waits for its sources — renaming a not-yet-written register cannot make
the value arrive earlier. Because such a class has no execution cost, the checker
rejects combining it with `uses`, `uop(...)` or a non-zero `latency`.

`zero_idiom = true` marks a class *eligible* to break dependencies. The decision
is per instruction instance: when the registers it reads are all among the
registers it writes, the instruction's value does not depend on its inputs, so
the engine drops its input dependencies and treats the instance as eliminated.
Every other instance of the same class executes normally with the bound latency
and micro-ops.

## Scheduling classes and binding

An ISA declares machine-independent **scheduling classes** — groups of operations
expected to behave alike and be handled by the same kind of hardware. The optional
defaults feed the fallback chain when no machine refines them:

```
sched_class WriteIALU { latency = 1; }
sched_class WriteLoad { latency = 3; }
```

An instruction opts into one or more classes in its `schedule` block:

```
instruction Load for [RV32I, RV64I] : LoadInst {
  ...
  schedule {
    units = [WriteLoad];
  }
}
```

A machine then binds each class to concrete cost. Timing is given **either** as a
scalar `latency` **or** phase-based via `reads`/`writes` naming pipeline stages,
which desugars to `latency = cycle(writes) - cycle(reads)` with a non-zero read
cycle. Scalar `latency = N` is equivalent to reading at cycle 0 and writing at
cycle N.

```
// Phase-based: operands read at ID, result written at EX (latency = 1) / MEM (= 2).
bind WriteIALU { reads = ID; writes = EX;  uses = [ALU]; }
bind WriteLoad { reads = ID; writes = MEM; uses = [LSU]; }
```

```
// Scalar form, plus a per-instruction override that supersedes the class-based
// resolution for one specific instruction on this machine.
bind WriteIALU { latency = 1; uses = [ALU]; }
override Add   { latency = 2; uses = [ALU]; }
```

A real device's bypass network is declared with `forward`: a result produced on the
`from` unit can be consumed on the `to` unit with the given producer→consumer
latency, instead of waiting for write-back.

```
forward ALU => ALU { latency = 0; }   // dependent ALU ops run back-to-back
forward LSU => ALU { latency = 1; }   // a load feeds an ALU op with a 1-cycle bubble
```

## Registers and register renaming

By default the number of physical registers equals the number of architectural
registers. Out-of-order machines rename to break false (WAR/WAW) dependencies,
drawing from a larger physical pool. A machine declares the pool size per **physical
register file**:

```
reg_file {
  GPR { count = 128; }
}
```

A *physical register file* is the root of a register class's inheritance chain
(`RegisterClass::register_file`). This is how aliasing is expressed without any new
syntax: AArch64 `GPRsp : GPR` shares the `GPR` file, so both classes name the same
physical register at a given index and draw from one `reg_file GPR` pool. Distinct
files (RISC-V integer `GPR` vs. float `FPR` vs. vector `VR`) are simply named
separately, each with its own `count`. A file a machine does not list defaults to
that file's architectural register count.

Renaming pressure is only meaningful where there is a renamer: the analyzer applies
`reg_file` capacities on out-of-order cores (stalling dispatch when a file's pool is
exhausted) and ignores them on in-order cores, which address architectural
registers directly.

## Scoreboarding and VLIW (design — not yet implemented)

Some machines delegate hazard resolution to the compiler instead of interlocking in
hardware. Latency is then a property of the *encoded program*, not something the
machine discovers at run time. Two mechanisms are common:

- **Scoreboarding / exposed pipelines.** The pipeline does not stall on data
  hazards; the compiler must separate a producer from its consumer by enough
  instructions (inserting `nop`s when it cannot). This is already expressible in
  spirit through `unprotected`/`hard` pipeline stages — an `unprotected` stage tells
  the model the latency is exposed and must not be interlocked. What is *not* yet
  expressible is the compiler-facing requirement (the minimum separation the
  scheduler must guarantee) or automatic stall-`nop` accounting.

- **VLIW.** Independent operations are packed into a fixed-width **bundle** that
  issues as a unit; `issue_width` would denote the number of bundle slots, and
  per-slot latency would be carried in the instruction encoding rather than resolved
  from a `sched_class`.

The intended TMDL surface for these — a way to mark a machine as compiler-scheduled,
to carry latency on the instruction encoding, to describe bundle slot constraints,
and to express explicit resource acquire/release — is **planned but not parsed in
this release**. Today, model such cores approximately with `unprotected` stages and
scalar latencies.

## Analyzing throughput with `tir sched`

`tir sched` statically estimates how a code region flows through a machine, similar
to `llvm-mca`. It does not execute the code: it reads the region's instructions,
reconstructs data dependencies on the fly from the physical registers each one
reads and writes (the same idea a hardware renamer uses), repeats the region a
number of iterations, and assigns dispatch/issue/retire cycles honoring
dependencies (forwarding-aware), functional-unit contention, issue width, the
reorder-buffer window, in-order vs. out-of-order issue, and register-file pressure.

```
tir sched --march rv64i --model rv64-ooo --iterations 100 region.s
```

The `--model` name is the globally unique machine name (e.g. `rv64-ooo`,
`arm64-in-order`). `--view` selects how results are presented — `resource` (the
default: totals/IPC, a per-instruction latency/reciprocal-throughput table, and
resource pressure per iteration and by instruction) or `timeline` (a per-cycle
trace of each instruction through dispatch/execute/retire). Both views are driven
by the same pipeline event stream, so new views can be added without touching the
engine. With no `--model`, it analyzes against the generic single-issue core, where
every instruction costs one cycle.
