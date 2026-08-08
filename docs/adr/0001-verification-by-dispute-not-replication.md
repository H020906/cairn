# ADR-0001 — Verify by dispute, not by replication

- **Status:** Accepted
- **Date:** 2026-08-07
- **Supersedes:** none

## Context

Cairn distributes computation to volunteer machines that cannot be trusted. Two distinct
failure modes must be caught:

1. **Deliberate fabrication.** A participant returns a plausible-looking answer without
   doing the work, to farm credit or to poison a result set.
2. **Silent corruption.** A participant does the work honestly on hardware that is
   defective, overclocked, or thermally throttled. Historically this has been the *more
   common* source of bad results in volunteer grids, and it is indistinguishable from
   fabrication at the protocol level.

The established answer, used by BOINC since 2002, is **redundant execution**: dispatch each
work unit to N independent hosts and accept the answer only when a quorum agrees. It is
simple, it works, and it is correct.

It is also enormously expensive. With the common N = 2 (escalating to 3 on disagreement),
**roughly half of all donated computing power is spent recomputing results that were
already correct.** Every volunteer's contribution is halved before it reaches any science.
For a project whose entire premise is harvesting wasted capacity, spending half of what we
harvest on insurance is the central inefficiency worth attacking.

## Decision

Cairn verifies **optimistically**, and pays the cost of verification only when a
disagreement actually occurs.

### 1. Every result carries a commitment to its own execution

A work unit is a deterministic WebAssembly program. The worker executes it once, at full
speed, on the host's native WASM engine, taking a snapshot of machine state every `2^k`
instructions. It returns the result **and a Merkle root over those snapshots**.

```
result = f(input)
commit = merkle_root([ state@0, state@2^k, ..., state@end ])
```

This is a few percent of overhead, not a second execution.

### 2. Confidence comes from sampling, not from repetition

- **Canary units.** Work units whose correct answer the coordinator already knows,
  injected into the assignment stream and indistinguishable from real work. They measure a
  worker's honesty *and* their hardware's integrity continuously, at a tunable rate.
- **Reputation.** A per-worker posterior on "returns correct results", updated from canary
  outcomes and settled disputes.
- **Selective replication.** Replication is not abolished — it is *targeted*. New workers,
  low-reputation workers, and high-value units still get a second execution. Established
  workers doing ordinary units do not.

### 3. Disagreements are settled by bisection, not by recomputation

When two commitments for the same unit differ, the coordinator does **not** re-run the job.
The two workers play an interactive bisection game:

```
Round 1        disagreement spans instructions [0, N)   → both reveal state@N/2
Round 2        disagreement spans one half              → both reveal its midpoint
...
Round log₂(N)  disagreement spans one instruction i
Adjudication   coordinator executes instruction i alone, on the instrumented
               interpreter, and learns which worker's state transition was wrong
```

Coordinator cost is **one instruction and `O(log N)` messages**, regardless of whether the
unit ran for a million instructions or a trillion.

The coordinator can execute that instruction without replaying anything because the parties
hand it a **state witness**: the small parts of the machine whole — globals, both stacks,
locals, the counters — and memory as only the pages that one instruction touches, each with a
Merkle proof binding it to the state root bisection already established. Rebuilding a
commitment from the witness and finding it equal to that root is what proves the witness was
not fabricated; nothing else needs checking, because the commitment covers every part of the
state.

An earlier draft of this ADR described that cost as `O(1)` compute. That overstates it. A
witness grows with the module's declared stack depth, local count and the number of pages one
instruction can reach — `memory.fill` can span a dozen. The claim that is both true and the
one the argument actually needs is narrower:

> **The coordinator's work is independent of the length of the disputed execution.**

Arbitrating a unit that ran for a trillion instructions costs what arbitrating one that ran
for a thousand costs. That is what makes cheating unprofitable at any scale, and it does not
require the stronger `O(1)` reading.

### 4. A worker that abandons a dispute loses it

Volunteers disconnect; that is normal, not adversarial. If a party fails to answer a
bisection round within its lease window, judgment defaults against them, their reputation
takes a bounded penalty, and the unit returns to the queue. The absent party is never
convicted of fraud on absence alone — the reputation penalty for silence is materially
smaller than the penalty for a proven false state transition.

## Expected cost

Let `s` = snapshot overhead, `c` = canary sampling rate, `r` = selective replication rate.
Steady-state cost per unit of useful science:

| Scheme | Cost multiplier |
|---|---|
| BOINC, N = 2 | ≈ 2.00× |
| Cairn | ≈ 1 + s + c + r |

With plausible values (`s` ≈ 0.05, `c` ≈ 0.03, `r` ≈ 0.10) that is ≈ **1.18×** against
≈ **2.00×** — roughly 1.7× more science from the same donated hardware.

**These are design targets, not measured results.** Establishing the real curve — error
rate as a function of `c`, `r`, and adversary fraction — is a deliverable of this project,
not an assumption of it. See the benchmark task in the roadmap.

## Consequences

**Enabling.** Bit-exact determinism becomes a hard requirement rather than a nice
property. If two honest workers can produce different traces, arbitration convicts honest
people and the entire scheme inverts. This constraint is recorded separately in
[ADR-0003](0003-determinism-constraints.md) and it is what forces a custom Rust execution
kernel ([ADR-0002](0002-language-boundaries.md)).

**Enabling.** The instrumented interpreter must agree with the browser's native WASM engine
bit-for-bit on every instruction. This is the single largest technical risk in the project
and is mitigated by differential fuzzing of the two engines as an always-on CI gate.

**Limiting.** Canary sampling is defeated by an adversary who can *distinguish* canaries
from real work. Canaries must therefore be drawn from the same workload and the same input
distribution as live units, and must not be reused across workers. Where indistinguishability
cannot be guaranteed for a given workload, that workload falls back to selective replication
at a higher rate.

**Limiting.** This reduces the trust placed in *workers*. The coordinator remains trusted —
it defines canaries, arbitrates disputes, and holds the results. Removing trust in the
coordinator is a genuinely harder problem and is explicitly out of scope for v1. Cairn
should not claim otherwise.

**Cost.** Substantially more protocol complexity than "run it twice and compare". A naive
replication path is retained and remains the fallback for any workload whose determinism
cannot be established.

## Alternatives considered

**Full replication (BOINC's design).** Rejected as the primary mechanism for the cost
reasons above. Retained as a fallback and as the baseline to benchmark against.

**Trusted execution environments (SGX/SEV attestation).** Would give strong guarantees, but
excludes the overwhelming majority of consumer hardware and browsers — which is precisely
the population Cairn exists to reach. Contradicts the project's premise.

**Zero-knowledge proofs of computation.** Cryptographically ideal: the worker proves
correctness with no interaction at all. Rejected because proving overhead for general
scientific computation currently runs many orders of magnitude above native execution.
This is the right answer eventually and the wrong answer today; the interface is designed so
a proof-carrying backend could be added later without changing the coordinator's contract.

**Trust nothing, recompute everything centrally.** Defeats the purpose of the project.
