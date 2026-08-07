# ADR-0003 — Enforcing bit-exact determinism by instrumenting the workload binary

- **Status:** Accepted
- **Date:** 2026-08-07
- **Depends on:** [ADR-0001](0001-verification-by-dispute-not-replication.md)

## Context

[ADR-0001](0001-verification-by-dispute-not-replication.md) settles disputes by locating the
first instruction at which two workers' execution traces diverge. That is only meaningful if
**two honest workers on different hardware, operating systems, and browser engines produce
byte-identical traces.** Non-determinism does not merely reduce Cairn's efficiency — it
manufactures disputes between honest participants and convicts one of them at random. The
scheme inverts.

So determinism is not a quality goal here. It is a correctness precondition.

WebAssembly is an unusually good substrate for this. Its core semantics mandate exact
IEEE 754 arithmetic with no extended intermediate precision and no FMA contraction — the
classic sources of "the same C program gives different answers on different CPUs" simply do
not apply. But the specification still leaves a small number of deliberate escapes, and each
one is fatal to us.

| Escape | Why it breaks Cairn |
|---|---|
| NaN payload bits are implementation-defined | Two engines produce different bits for the same operation |
| `memory.grow` failure depends on host RAM | A worker on a small machine diverges from one on a large machine |
| Stack overflow depends on host stack depth | Same |
| Threads, atomics, shared memory | Interleaving is nondeterministic by construction |
| Relaxed SIMD | Nondeterministic by explicit design of the proposal |
| Any host import exposing clock, entropy, or I/O | Trivially divergent |

## The problem with fixing this in the engine

The obvious fix is to make our interpreter canonicalize NaNs, bound the stack, and so on.
That works for the **slow path** — our own instrumented interpreter, used during
arbitration.

It does not work for the **fast path.** On the fast path we hand the module to the browser's
own WASM engine to get native speed. We cannot reach inside V8 or SpiderMonkey to normalise
a NaN payload after each floating-point operation, and we would not want to: interposing per
instruction is precisely the overhead the fast path exists to avoid.

If the two paths differ on any edge case, arbitration produces wrong verdicts. This is the
single largest technical risk in the project.

## Decision

**Determinism is enforced by rewriting the workload binary once, at registration time, not
by constraining the engine at run time.**

When a workload is submitted, the coordinator runs it through a transformation pass that
produces a *Cairn-canonical module*. Both the fast path and the slow path then execute the
**identical instrumented binary**, so there is no divergence to reconcile between engines.

The pass performs four jobs:

1. **NaN canonicalization.** Every floating-point instruction that can produce a NaN is
   followed by a canonicalization sequence forcing a single agreed bit pattern. Engines then
   have nothing left to disagree about.
2. **Fuel metering.** An instruction counter is incremented on entry to each basic block by
   that block's instruction count. Fuel is the trace's coordinate system: "instruction *i*"
   must mean the same thing on every machine.
3. **Snapshot hooks.** At each `2^k` boundary the module calls out to the host to commit the
   current state.
4. **Deterministic resource limits.** Memory ceiling and maximum call depth are baked into
   the module from the workload manifest and checked in-band, so exhaustion happens at the
   same instruction everywhere rather than when a particular machine happens to run out.

A **validator** rejects any module using threads, atomics, shared memory, relaxed SIMD, or
importing anything outside Cairn's own narrow host interface. That interface exposes exactly
three capabilities: read input, write output, snapshot. No clock. No entropy. No I/O.

### What a snapshot actually contains

Hashing the full linear memory every `2^k` instructions would cost `O(memory)` per snapshot
and dominate execution. Instead, machine state is maintained as an **incremental Merkle tree
over 64 KiB memory pages**, plus globals, operand stack, call stack, and program counter.
Pages are dirty-tracked; a snapshot rehashes only pages written since the previous one and
recomputes the affected path to the root. Steady-state cost is proportional to *writes*, not
to *memory size*.

This structure is also what makes single-instruction arbitration possible: the coordinator
can be handed one page plus a Merkle proof and verify a single state transition without ever
holding the whole memory image.

## Consequences

- Workloads cannot use threads or wall-clock time. For the embarrassingly-parallel
  scientific kernels Cairn targets, this costs nothing — each work unit is single-threaded
  by design and parallelism lives at the unit level.
- Instrumentation overhead is real: NaN canonicalization plus fuel metering plus snapshots.
  Measuring this honestly against uninstrumented execution is a required benchmark, not an
  optional one. If it exceeds the cost it saves over replication, ADR-0001 is wrong and must
  be revisited.
- The instrumentation pass becomes trusted code. A bug there corrupts every result on the
  network. It gets the project's most stringent testing.
- **Differential fuzzing is a permanent, always-on CI gate**, not a milestone: randomly
  generated modules are executed on the fast path and the slow path, and any divergence in
  final state, trace root, or fuel count fails the build. This is the only thing standing
  between us and silently convicting honest volunteers.

## Alternatives considered

**Trust that engines agree in practice.** Mainstream engines do follow the IEEE 754
recommended NaN behaviour most of the time. "Most of the time" is not a foundation for a
protocol that punishes disagreement, and the failures would be rare, silent, and
concentrated on unusual hardware — exactly the volunteers we can least afford to alienate.

**Run only the slow path everywhere.** Perfectly deterministic and far too slow; an
interpreter gives up roughly an order of magnitude, which would consume the entire
efficiency gain that motivates ADR-0001.

**Native binaries in containers instead of WASM.** No deterministic execution semantics, no
browser story, and a far larger sandbox-escape surface on volunteer machines. WASM is chosen
here for its *semantics*, not for its portability.
