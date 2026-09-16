# M01 semantic runtime

This document records the implementation design for milestone 1 of the
isasim runtime plan. The strict acceptance manifest is `xtask/isasim-accept/m01.toml`.

## Caller usage

Existing callers continue to use `Executor::run` and `run_with_trace`.
The interpreter drives a frame until it completes or faults:

```text
frame = instruction.start(entry_registers, instruction_pc)
loop:
    match frame.resume(response):
        effect(request): response = memory.service(request)
        complete(writes): commit(writes); break
        fault(record): report(record); break
```

An unaccepted request may wait. An accepted request has one response, and a
response must match the pending instruction and sequence. Resumption continues
the suspended expression; it does not execute the instruction again.

## Ownership

The symbolic evaluator owns its expression stack, memoized values and lambda
bindings. Conditional expressions evaluate only their selected branch. The
instruction frame owns the ordered statement stack, local bindings and
provisional register and PC writes. Existing generated `Program`, `Effect`,
register ports and implicit register metadata provide the semantic descriptor.
`MachineInstruction::info()` exposes these generated tables; register classes
carry file, view and group information. FP flags use the same fixed-register
write staging as other architectural registers. No second generated descriptor
format is introduced.

The address space owns permissions and mappings to backing identities. The
memory owner stores sparse backing pages. Mapping generations change on mapping
and protection changes. A scalar access validates its entire range before
observing or modifying bytes. The flat CLI window becomes an explicit mapping,
and untouched bytes remain zero without allocating the whole window.

The synchronous execution API drives the same frame with a `MachineContext`
adapter. Whole-range writes use `MachineContext::write_memory_bytes`; the default
rejects unsupported wide writes before changing bytes, and `Executor` validates
and writes their complete range. Multi-access memory visibility remains the
caller's responsibility. `Executor` uses the memory service and commits staged
stores before publishing registers. Dynamic timing replay continues to consume
the existing trace and
the scheduling class selected from instruction-entry values. Wide memory effects
remain one semantic request, while the legacy memory trace retains its
word-sized chunks for the existing cache model.

## Fault and restart policy

An ordinary scalar instruction commits registers and PC only after completion.
Its fault preserves earlier instructions and records the instruction PC and
effect sequence. Wide contiguous accesses validate their entire byte range.
Multi-access RAM instructions run only after every selected address and size
can be computed without external effects and every range passes permission
checks. This preserves successful AArch64 pair operations. An inaccessible
second range rejects the pair before the first effect, with sequence zero.
Device ranges, effect-dependent addresses or branches, effectful vector loops,
multiple atomic or ordering effects, and loads after staged stores are rejected
before effects. M01 does not implement architectural partial-completion or
restartable string/vector operations.

Read-to-clear devices accept scalar reads once. Backpressure does not accept a
request; a repeated accepted token is an error. Requests carry instruction and
sequence identities. The service assigns memory-order indices to observations
and immediate atomics when accepted, and to staged stores when committed.
A mapping-generation change invalidates an active instruction and LR/SC
reservations. An SC attempt consumes the reservation even if its write faults.
Ordinary stores preserve the existing simulator reservation rule. Unmapping the
last alias retires a read-to-clear device; raw mapping changes are pruned before
the next instruction or device allocation.

An exception request invokes the configured synchronous handler once. A handler
may continue or halt; an unhandled exception faults. Handlers see instruction-entry
register state. Multi-effect exception continuations are rejected in M01.
The existing CLI syscall handler remains a compatibility mechanism, outside a
new user-mode environment or transactional host-I/O contract.

## Alternatives

Replaying an instruction with a response cache would repeat traversal and could
repeat effects inside lambda bodies or branches. An explicit continuation keeps
the actual evaluation position and completed values instead. Copying all memory
per instruction would hide ownership and scale with mapped size; provisional
writes and a sparse memory owner avoid that cost.

## Verification

Add one failing public behavior test before each implementation unit. Cover
memory permissions and boundaries, nested effects, suspension and response
identity, precise faults, wide values and multi-access policy. Run the complete
existing simulator suite, including RV32, FP state, register aliases, atomics,
vectors and conditional timing. The acceptance dispatcher must report missing
tools and skipped required cases as failures in strict mode.

## CLI and acceptance artifacts

Both the existing command and `tir-isasim snippet` accept `--engine interp` and
`--events events.json`. Use `--events -` for stdout. Events identify PC,
instruction, sequence, request, response and memory-order position. Faults
include PC, sequence, access kind, address and cause. This is functional
execution with the existing optional trace-replay timing model.

Memory configuration JSON can replace the flat window with `mappings` entries
containing `start`, `size` and `permissions`, and read-to-clear `devices` entries.
Data `regions` retain the existing deterministic initializer. Code holes receive
execute/read mappings; existing mappings retain their permissions.

`cargo xtask isasim-accept m01 --strict` writes the manifest snapshot, case logs,
fixture hashes, tool versions, revision, dirty-workspace hash and output hashes
to `target/isasim-accept/m01`. The report also hashes the simulator binary.
Required cases cannot be skipped. Its manifest
covers all simulator LIT cases, simcore units, symbolic execution units and the
conditional-latency baseline.
