# ADR-0005 — The fast path cannot snapshot, so the trace moves to dispute time

- **Status:** Accepted
- **Date:** 2026-08-08
- **Supersedes:** the execution model in [ADR-0001](0001-verification-by-dispute-not-replication.md) §1, and the remediation named in [ADR-0004](0004-measured-cost-supersedes-the-efficiency-claim.md)
- **Superseded by:** none

## Context

Every document in this repository describes two execution paths:

- the **fast path** — the volunteer's own WebAssembly engine (V8, SpiderMonkey, wasmtime),
  running at native speed and taking a snapshot of machine state every `2^k` instructions;
- the **slow path** — Cairn's instrumented interpreter, used only to adjudicate one
  instruction during a dispute.

The slow path is built. The fast path was never started, and while planning it, it became
clear that **it cannot be built as specified.**

### What a snapshot has to contain

`state::StateCommitment` covers seven things, because arbitration has to be able to rebuild
the machine at an arbitrary instruction and step it:

| Field | Visible to a host embedding a stock WASM engine? |
|---|---|
| `memory` | **Yes** — exported memories are readable |
| `globals` | **Yes** — exported globals are readable |
| `segments` (which data/element segments were dropped) | No |
| `operand_stack` | **No** |
| `call_stack` — per frame: function, return position, locals | **No** |
| `program_counter` | **No** |
| `fuel` | Yes, but only because we inject the counting ourselves |

A WebAssembly embedder API is deliberately a black box over execution. It hands you exports
and takes host calls; it does not expose the value stack, the locals of any live frame, the
frame chain, or the instruction pointer. This is not an oversight in any particular engine —
it is what allows engines to compile WebAssembly into machine code at all, since none of
those structures need to exist in a recognisable form at runtime.

The proof is already in this repository. `runtime/tests/differential.rs` runs modules under
`wasmi` and implements `cairn.charge` as a host function. Inside it, the `Caller` handle can
reach the exported memory — and that is the entire extent of it. There is no API by which
that callback could learn the operand stack it was called with, and there is no engine in
which there would be.

**So the fast path can observe two of the seven fields.** A commitment built from those two
is not comparable with the interpreter's, which means the two paths can never be checked
against each other, which means the fast path cannot produce the trace the dispute protocol
consumes.

## Decision

**The honest path stops producing traces. A trace is produced on demand, by re-execution,
only when a result is disputed.**

Concretely:

1. A volunteer runs the module instrumented for **determinism only** — NaN canonicalization,
   validated feature set, resource ceilings. No fuel metering, no snapshots. They return the
   **result**.
2. Disagreement is detected by comparing **results**, as BOINC does. This needs nothing from
   inside the engine.
3. When two results for a unit differ, both parties are asked to produce a trace. Each
   re-executes the same work unit under the **fully instrumented** module — metering,
   snapshots, the lot — and returns the trace commitment.
4. Bisection and adjudication proceed exactly as they do today. Nothing in `dispute.rs`
   changes.

### Why this is sound

**Determinism was already a hard requirement**, for reasons that have nothing to do with this
ADR ([ADR-0003](0003-determinism-constraints.md)). Given it, re-executing a unit *is* the
original execution — not a reconstruction of it, the same one. A trace produced at dispute
time describes what happened at submission time because nothing else could have happened.

**The result binds the trace.** A worker who submitted a fabricated result must produce a
trace whose final state yields that result. No honest execution does, so they must fabricate
the trace too — and fabricating a trace is precisely what bisection catches.

**The two instrumentation levels must be the same program.** This is the new load-bearing
property, and it is now tested rather than assumed: `assert_instrumentation_is_transparent`
runs every case in the differential corpus under both the determinism-only module and the
fully instrumented one, under **both** engines, and asserts identical output and identical
trapping. If metering could change an answer or turn a completed run into a trap, arbitration
would settle a dispute about an execution that never happened — and it would do so against an
honest worker.

## Consequences

**Enabling — the verification tax leaves the honest path.** Measured, on three of four
benchmark workloads: honest-path overhead is now **indistinguishable from zero** (±2%),
against +20% to +42% when the same workloads carried metering and snapshots. See
[benchmarks.md](../benchmarks.md).

**Enabling — snapshot interval stops being a cost dial.** `k` no longer trades honest-path
speed against bisection granularity; it only affects disputed units. It can be set for
arbitration quality alone.

**Limiting — floating point is untouched, and is now the whole problem.** The float kernel
still costs **+150%** on the honest path, and its instruction count is 2.30× bare. Every bit
of that is NaN canonicalization, which is determinism instrumentation and cannot be deferred:
it is what makes two honest workers agree in the first place. Cairn's cost problem was
"verification is expensive"; after this ADR it is **"bit-exact floating point is expensive"**,
which is a different and more tractable problem — most float operations in a numeric kernel
cannot produce a NaN from non-NaN inputs, and proving that statically would remove the check
rather than optimise it.

**Limiting — a dispute now costs two re-executions.** Previously the traces already existed;
now they must be made. This is the intended trade: the rare path gets more expensive so the
common path gets cheaper, which is the same argument ADR-0001 made against replication, just
applied one level deeper.

**Limiting — silent internal divergence is no longer detected.** Two workers whose executions
differed internally but produced the same result now agree. This was never worth much:
a divergence that does not change the result does not change the science, and hardware faults
that *do* change results are caught by canaries and by the result comparison itself.

**Risk — a defendant can now retreat into silence.** Under the old design the trace commitment
was submitted up front, so a liar was already on the record. Now a challenged worker can
simply not answer, and take the absence penalty instead of a fraud conviction. ADR-0001 makes
the absence penalty deliberately smaller than the fraud penalty, which would make lying
approximately free.

**This is closed by a rule, not by a mechanism:** *you may go silent as a witness, never as a
defendant.* A party asked to corroborate someone else's unit may drop out with the small
penalty, because volunteers genuinely disconnect. A party asked to produce a trace for a
result **they themselves submitted** and who fails to produce it is treated as convicted. They
made a claim; declining to support it is not neutral.

## What this replaces in earlier ADRs

**ADR-0001 §1** says the worker returns "the result **and** a Merkle root over those
snapshots", at "a few percent of overhead". Both halves are withdrawn: the worker returns the
result, and the overhead figure was never right ([ADR-0004](0004-measured-cost-supersedes-the-efficiency-claim.md)).
The *mechanism* ADR-0001 describes — bisect to one instruction, adjudicate from a witness,
cost independent of execution length — is unaffected and remains correct.

**ADR-0004's named remediation is withdrawn.** It proposed replacing the `cairn.charge` call
with a global counter plus a threshold test, on the reasoning that a host-boundary call is
expensive. Two things retired it. First, metering is no longer on the honest path, so its cost
now only affects disputed units — a rare path where correctness matters more than speed.
Second, the reasoning could not be checked: the per-workload arithmetic in ADR-0004's own
figures put the cost of an injected `charge` pair at roughly 25 ns on three workloads and 90 ns
on the fourth, and the attempt to pin that down instead revealed that **the benchmark harness
could not resolve it at all** — see the *Noise floor* section of [benchmarks.md](../benchmarks.md),
where two byte-identical modules time up to 148% apart on the workload in question. It may
still be a good change. It is not currently a justified one, and it would be measured against
an instrument that cannot see it.

## Alternatives considered

**Materialize the machine state into linear memory.** Rewrite the module so the operand stack
and every frame's locals live in a shadow stack in linear memory, which the host *can* read.
This is what Arbitrum's WAVM and Optimism's Cannon do, and it genuinely works. Rejected: every
local access becomes a load and store, which is a large constant-factor tax on *all*
execution — and the fast path exists precisely to avoid a constant-factor tax. It would trade
the problem this ADR solves for a worse version of it.

**Snapshot only where the operand stack is empty.** Cheap and appealing, since the
instrumenter knows the static stack shape at every point. Insufficient on its own: the locals
of every live frame are still invisible, and so is the frame chain. It solves one of the four
missing fields.

**Run the interpreter on the fast path too.** Then everything is observable — and Cairn is a
slow interpreter distributed to volunteers, which is the opposite of the project's premise.

**Have the host reconstruct state by replaying from the last snapshot.** Circular: you cannot
take the last snapshot.
