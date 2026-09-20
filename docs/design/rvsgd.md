# RVSDG and control flow

A compiler needs to preserve which computations run, which values they produce,
and the order of their effects. TIR expresses those relationships in two forms.
A control-flow graph, or CFG, uses ordered basic blocks connected by branches.
A Regionalized Value State Dependence Graph, or RVSDG, uses nested regions
connected by value and state dependencies.

TIR converts function bodies from CFG form into unordered regions for
optimization and instruction selection. It then builds blocks containing the
selected machine instructions. The two conversions preserve the computation
and its effects. The resulting blocks need not match the original blocks.

This chapter explains those conversions and their place in FCC, TIR's C
compiler. The [Core IR chapter](core_ir.md) introduces the storage shared by
both forms. The [original RVSDG paper](https://arxiv.org/html/1912.05036v2)
provides background on regional value and state dependencies.

## Regions express the structure of a computation

An operation consumes values and produces results. A region contains operations
and exposes results of its own. Some operations own nested regions. A function
owns its body, a conditional owns its alternatives, and a loop owns its body.

In CFG form, each basic block contains an ordered sequence of operations.
A terminator ends the block with a branch or a return. Branches can pass values
to the next block's arguments.

In RVSDG form, a region contains unordered operations. Their position in the
text does not determine execution order. If a multiplication consumes an
addition's result, the addition must run first. Independent computations have
no order unless another dependency imposes one.

Region arguments are also called ports. The enclosing operation supplies their
values. A nested region can read visible values from its enclosing scope, while
its results expose values to the enclosing operation. A value produced in one
conditional alternative becomes available outside that alternative through the
conditional's results.

TIR uses three SCF operations to express this structure:

- `scf.switch` selects one alternative and produces that alternative's results.
  Its integer predicate indexes the alternatives. A value beyond the last
  index selects the last alternative.
- `scf.loop` carries values through repeated evaluations of a body region.
  The body produces a repeat predicate, values for the next iteration, and
  values to return when the loop stops.
- `scf.for` is a counted loop. It makes the counter, bounds, and step explicit
  so loop transformations can use them.

The `Gamma` interface describes a conditional's predicate and alternatives.
The `Theta` interface describes a loop's predicate and carried values. Passes
use these interfaces to read the bindings between operands, ports, and results.
Both `scf.loop` and `scf.for` implement `Theta`.

## State dependencies preserve effects

Value dependencies alone cannot order a store and a later load from the same
address. TIR uses state values to express dependencies on resources such as
memory. A memory operation observes an incoming state and produces a state
that later operations can depend on.

For example, a store of `1`, a load, and a store of `2` must preserve the load's
observation of `1`. The state dependencies preserve that order after the block
containing those operations disappears.

```mermaid
flowchart LR
	S["Incoming memory state"] --> W1["Store 1"]
	W1 -->|state| R["Load"]
	R -->|state| W2["Store 2"]
	R -->|value| U["Use the loaded 1"]
	W2 --> F["Outgoing memory state"]
```

The conversion constructs missing dependencies while the original block order
is still available. It identifies memory objects and builds separate chains
where the accesses permit them. Memory with unknown provenance uses a world
chain. Accesses that may refer to the same memory must remain ordered against
each other.

Reads can share the state left by a preceding write. Their outgoing states
join before a subsequent write that must follow all those reads. An operation
that affects several chains joins their states. The conversion splits the
resulting state when later operations need to name the chains separately.

State dependencies cross conditionals and loops along with ordinary values.
Region state results keep required effects connected to the function's result.
The unordered representation therefore retains the order that matters without
retaining the order of every operation in a block.

## CFG restructuring builds nested regions

The `restructure-nodes` pass converts an ordered function body into unordered
regions. A function that already has an unordered body needs no conversion.
A single ordered block still needs conversion, even when it contains no branch.

The pass first builds a temporary graph. Each node refers to an original block,
and each edge records a destination and assignments to its arguments. Values
used across blocks become typed variables in this graph. Generated control
selectors use the same variable representation. Compatible function exits
converge on one exit node.

The pass structures loops first, then branches, and finally emits the regions.
Original computations move into their new regions without being copied.

### Loops converge on one entry and one exit

A cycle in a CFG represents repeated execution. The pass uses Tarjan's
algorithm to find strongly connected components: sets of blocks that can all
reach one another. Cyclic components identify the parts that need loop
structure.

When a component already has a suitable tail decision, the pass preserves that
decision as the repeat predicate. Otherwise, it introduces a common head and
tail. Selectors record which original entry an incoming edge wanted, whether
the iteration repeats, and which exit to take when it stops.

Consider a cycle that can be entered at either block A or block B. The incoming
edge supplies an entry selector. One loop head then dispatches to A or B,
preserving the original choice without copying either block. A loop with
several exit destinations uses an exit selector to make the corresponding
choice after the loop.

The pass repeats this process inside each loop body to form nested loops.
The enclosing graph treats a completed loop as one unit and follows its exit
when it looks for the surrounding structure.

### Branches converge on a continuation

A structured conditional has alternatives followed by a common continuation.
For a CFG branch, the pass examines the blocks reachable from each alternative
to find where their paths meet.

When the paths meet at one entry to their shared continuation, that entry is
the join. When several joins are possible, the pass creates a selector and a
common dispatch. Each incoming path records which continuation it wanted.

```mermaid
flowchart TD
	B["Original branch"] --> L["Left alternative"]
	B --> R["Right alternative"]
	L -->|"Select continuation X or Y"| J["Common join"]
	R -->|"Select continuation X or Y"| J
	J --> D{"Continuation selector"}
	D --> X["Continuation X"]
	D --> Y["Continuation Y"]
```

The alternatives keep their original computations. The extra selector makes
their control choice a value that can cross a region boundary. The result of
branch restructuring is a tree describing the nested conditionals and loops.

### Live values determine the region results

Restructuring must also preserve values passed between blocks. The pass
computes liveness by tracing which variables later operations and edges read.
A variable is live at a point when its current value is still needed there.

A conditional produces a result for a variable that an alternative assigns
and the continuation needs. Values available in the enclosing scope can be
read directly inside an alternative. The emitted conditional forwards state
dependencies through explicit ports.

A loop carries the variables its body assigns that are needed by another
iteration or after the loop. Its initial operands supply the first iteration's
ports. The body then supplies separate continuation and exit values.

Emission turns the temporary variables back into value references and region
bindings. It creates `scf.switch` and `scf.loop` operations for the tree, and
converts preserved counted loops into `scf.for`. The function's return values
and outgoing states become region results. An unordered region has no return
terminator inside it.

### Input boundaries

The CFG reader uses the `Terminator`, `BranchTerminator`, and `BranchGuard`
interfaces. It accepts unconditional branches and two-way conditional branches
whose guard describes both edges. Branches with more than two successors need
lowering before this conversion.

The input also needs an exit node. Multiple exits must have compatible
terminators and operands so the pass can combine them. These requirements
apply to the input representation, including graphs with multiple-entry loops
or several loop exits.

## Reconstruction places demanded computations in blocks

The shared `destructure` routine turns unordered regions into CFG blocks.
It starts from a region's results and follows their dependencies. These
dependencies determine which operations must execute to produce the results.
The routine places those operations in topological order, so each computation
follows the computations it depends on.

A dependency cone is the set of operations needed to produce a particular set
of values. The cone includes state dependencies as well as ordinary values.
An unused pure computation needs no place in the output. An operation with a
state result that no region result demands is an error, because dropping that
operation would lose an effect.

### Conditionals become tests, alternatives, and a merge

For a `Gamma` operation, reconstruction creates tests that select an
alternative. Each alternative computes its results and transfers them to a
merge block. The merge block's arguments take the place of the conditional's
results.

An alternative with no demanded computations can pass its results directly to
the merge. A test already resolved by the caller can also eliminate an
unreachable alternative. The routine creates blocks for the work that remains.

### Loop paths determine where computations run

A `Theta` body exposes a predicate, continuation values, and exit values.
Reconstruction finds the dependency cone of each group. The loop header holds
the predicate's cone and the computations needed by both continuation and exit.
The remaining computations belong only to their selected path.

```mermaid
flowchart TD
	I["Initial carried values"] --> H["Header: predicate and shared dependencies"]
	H --> P{"Repeat?"}
	P -->|Yes| C["Continue-only computations"]
	C -->|"Next carried values"| H
	P -->|No| E["Exit-only computations"]
	E --> M["Merge: final loop results"]
```

This placement preserves conditional evaluation. A load needed only to prepare
the next iteration does not execute when the predicate says to stop. A store
needed only on exit executes when the loop stops. Computations shared by both
paths run before the decision because either outcome needs them.

The diagram shows the general shape. Empty continuation or exit computations
need no separate block. Nested structured operations introduce their own
blocks within the path that evaluates them.

### The caller supplies the branch operations

Reconstruction uses an `Edges` interface to emit jumps, conditional branches,
and function exits. The standalone `destructure` pass supplies `CfgEdges`,
which creates generic `cfg` operations. The backend supplies `MachineEdges`,
which creates target branches from the instruction selector's choices.

Both callers use the same region bindings and dependency rules. The backend
can use a selected comparison directly as a branch test and preserve implicit
machine dependencies alongside explicit value dependencies.

## FCC keeps regions through instruction selection

FCC initially represents ordinary control flow as blocks and branches. Its
C-specific `cir` dialect provides loop operations that preserve the source
condition, body, and step regions where the frontend can retain that structure.

```mermaid
flowchart TD
	C["C frontend: CFG and CIR loops"] --> L["Lower structs and raise loops"]
	L --> R["restructure-nodes"]
	R --> U["Unordered SCF regions"]
	U --> O["Optimize values, loops, and state dependencies"]
	O --> S["Select instructions within regions"]
	S --> D["Destructure with machine branches"]
	D --> B["Machine blocks"]
	B --> A["Allocate registers and emit code"]
	U -. "Standalone destructure pass" .-> G["Generic CFG"]
```

Before restructuring, `raise-loops` recognizes counted loops and represents
them as `scf.ordered_for`. The restructuring pass converts those operations
to unordered `scf.for` bodies. Retaining the counter and bounds gives
[affine loop scheduling](affine.md) the structure it needs.

Loops that `raise-loops` cannot prove counted become CFG blocks and take the
general restructuring path. A label or `goto` anywhere in a function, or a
return inside a loop, makes the frontend flatten all loops in that function.

FCC runs struct lowering, loop raising, and restructuring at every optimization
level. Optimizing levels then run inlining, memory-slot promotion, dependency
verification, and simplification over the unordered representation. Inlining
replaces a call with the called function's computation. Promotion replaces
eligible local loads and stores with value dependencies. At `-O2` and `-O3`,
affine scheduling can reorder counted loops or group their iterations into
tiles while respecting dependencies. Machine emission at `-O0` still runs the
normalizing simplifier. An `-O0` IR dump shows the frontend conversion before
that simplification.

Instruction selection processes the nested unordered regions. After it commits
the selected instructions, it calls the shared destructurer with machine
branches. Each resulting block then receives an order consistent with its
machine dependencies. Register allocation and final emission consume those
blocks. The [Instruction selection chapter](isel.md) explains how TIR chooses
the instructions.

The shared backend also accepts raw CFG input from other clients. Its prologue
runs `restructure-nodes` before selection and verifies the state dependencies.
Functions that already have unordered bodies retain their existing structure
and dependencies.
