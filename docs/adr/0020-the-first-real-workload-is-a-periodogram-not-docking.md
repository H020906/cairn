# ADR-0020 — The first real workload is a periodogram, not molecular docking

- **Status:** Accepted
- **Date:** 2026-08-13

## Context

`ARCHITECTURE.md` has said since the scaffold that `workloads/` is *"real science, compiled to
WASM. The first target is molecular docking for virtual drug screening: one compound per work unit,
embarrassingly parallel, deterministic, and genuinely useful."* The roadmap's last phase-C item
said the same. `docs/MAINTAINER.md` listed it under what does not exist yet.

That intention was written before anything else in this repository existed, and the thing it was
picked for is right: docking *is* embarrassingly parallel, it *is* one compound per unit, and it
*is* useful. What it is not is a good first real workload for Cairn, and two of the reasons only
become visible once the rest of the system is built.

**A docking unit is not self-contained.** Scoring a ligand against a receptor needs the receptor —
a protein structure of tens of thousands of atoms, the same for every unit in a campaign. Cairn's
unit model is *bytes in, bytes out*: the input is carried in the unit, hashed into its identity,
and journalled. Megabytes of receptor repeated per unit would be absurd, and the alternative —
compiling the receptor into the module — makes a new module per target and moves the problem into
the workload identity instead of solving it. **Shared reference data is a real feature Cairn does
not have**, and discovering that while trying to ship a first workload would have meant either
building it in a hurry or pretending the workload was finished.

**A docking score cannot be checked against anything but itself.** Scoring functions are empirical,
heavily parameterised, and calibrated against experiment; there is no closed form saying what the
answer should be. So the only available test is *"three engines agree"* — and three engines
agreeing is exactly what a wrong kernel also does. That is a bad property for the workload whose
job is to demonstrate that the grid computes something real.

## Decision

**A Lomb–Scargle periodogram over one frequency band.**

Given observations `(t, y)` at uneven intervals — what a telescope produces, because of daylight,
weather and scheduling — it estimates the periodic power at each of a set of frequencies. A peak is
a candidate period: a variable star, a binary, a transiting planet, a pulsar. It is the standard
tool for the job (Lomb 1976, Scargle 1982) and it is the shape of search Einstein@Home actually
runs.

It was chosen against three criteria, in this order:

1. **A unit is a frequency band, and bands are independent.** No shared state, no communication, no
   ordering, and — unlike docking — no reference data. The observations are small enough to carry
   in the unit, and splitting a search means splitting the band.
2. **The answer can be checked against something outside the computation.** Synthesise a signal at
   a known frequency, scan a band containing it, and the peak has to come back at that frequency.
   `a_periodogram_recovers_a_known_period_and_every_engine_agrees` asserts both halves: three
   engines identical bit for bit, *and* the science right. **Every other test in that file checks
   only the first**, and three engines computing the same wrong number satisfy it perfectly.
3. **It is almost entirely `sin` and `cos`**, which makes it the hardest possible exercise of
   [ADR-0016](0016-math-belongs-in-the-module-not-the-host.md)'s math library and the workload most
   likely to expose a float divergence if one existed.

That third point is also the ordering argument this project made a year of decisions around, now
paid off. A real workload built on **host** trigonometry would have manufactured disputes at
roughly the rate it called `sin` — and it would have presented as an unexplained dispute rate on a
computation nobody could independently check. The math library had to come first, and it did.

## Consequences

**What it buys.** `workloads/examples/sum-of-squares.wat` stops being the thing every demonstration
is about. The periodogram is admitted by the gate, runs on all three engines with identical bytes
and identical fuel, and recovers the period it was given.

**It exercised the whole of phase C and needed no changes to any of it.** The workload is built
from `workloads/template` (C3), calls `cairn-math` for every trigonometric function (C1), and was
admitted first try — which is the evidence that C2's widened gate covers what a real compiler emits
for real code, rather than only what the probes emit.

**What it costs.** Molecular docking remains an intention, and `ARCHITECTURE.md` now says so with a
pointer here rather than presenting it as the plan. The two things it needs before it is tractable
are named above: shared reference data per campaign, and an answer that can be checked against
something.

**What this workload does not claim.** The result is not the mathematically exact periodogram. It
is a specific, reproducible sequence of `f64` operations that approximates one — floating-point
addition is not associative, so the order of every sum is part of the answer, and the loops are
sequential over the input in the order it arrived. **Cairn verifies that two volunteers computed
the same thing; whether that thing is the right science stays the author's problem.** A more
accurate summation would be a *different* answer rather than a better one, and would be fine — but
only if every volunteer changed at the same instant, which the unit id being a hash of the module
bytes is what enforces.

## Alternatives considered

**Molecular docking anyway, with a small receptor.** Rejected: a receptor small enough to carry per
unit is not a receptor anybody docks against, so the demonstration would be of the same shape as
`sum-of-squares` — a fixture wearing a lab coat.

**Smith–Waterman sequence alignment.** Genuinely useful, genuinely parallel, self-contained, and
checkable against a reference implementation. Rejected because it is integer dynamic programming
and touches no transcendental function at all, so it would exercise none of what phase C built and
would say nothing about the risk phase C was managing.

**N-body integration.** Rejected on the first criterion: a step depends on the previous step, so a
unit is not independent of its neighbours. That is the one property this grid cannot supply.

**Monte Carlo integration.** Self-contained, parallel, uses transcendentals, and checkable against
an analytic integral. It was the closest runner-up and was rejected as being *about* Cairn rather
than about science — the interesting content would have been the counter-based deterministic
generator, which is a distributed-computing exercise rather than a scientific one.
