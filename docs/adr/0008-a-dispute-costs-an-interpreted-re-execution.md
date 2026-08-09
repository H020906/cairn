# ADR-0008 — A dispute costs the parties an interpreted re-execution, and that is the real number

- **Status:** Accepted
- **Date:** 2026-08-09
- **Corrects the practical import of:** [ADR-0007](0007-metering-is-a-jit-problem-not-an-interpreter-problem.md)

## Context

[ADR-0007](0007-metering-is-a-jit-problem-not-an-interpreter-problem.md) measured fuel metering
at **+484% to +505%** under wasmtime, against 18%–41% in Cairn's interpreter, and reinstated
the plan to replace the injected `cairn.charge` call with a global counter.

Then a question that should have been asked first: **who runs the fully instrumented module?**

Nobody runs it on a JIT. The reason is the one [ADR-0005](0005-the-fast-path-cannot-snapshot.md)
already established — a trace commitment covers the operand stack, every frame's locals, the
frame chain and the program counter, and no host engine exposes any of them. That argument does
not stop applying because the module is metered. **A challenged party cannot produce a trace on
the engine they did the work with either. They must re-execute under Cairn's interpreter**,
which is exactly what `dispute::Replay` does.

So ADR-0007's headline figure prices a configuration that never occurs.

## What the cost actually is

The gap between the two engines on the honest module, which is the multiplier a party pays to
switch engines at dispute time:

| workload | honest path, JIT | same, interpreter | **ratio** |
|---|---:|---:|---:|
| integer loop | 2.5 ms | 282.2 ms | **112×** |
| float kernel | 0.83 ms | 106.6 ms | **129×** |
| memory sweep | 1.9 ms | 69.9 ms | **37×** |
| recursion | 1.4 ms | 197.3 ms | **142×** |

Against that, metering's 18%–41% is a rounding error. The instrumentation was never the
expensive part of a dispute; **the change of engine is.**

A challenged party pays it more than once:

1. **Producing the trace** — one interpreted execution: ~40×–140×.
2. **Answering `log₂(n)` bisection rounds** — `Replay` restarts from the beginning each round,
   so `O(n log n)`. A real worker would keep periodic full-state checkpoints and resume from
   the nearest, making it `O(n)`; the code says so where it happens. Even at `O(n)` that is
   another interpreted execution's worth.

Call it **~200× a normal execution, per party, per dispute**, with the second half improvable
to about 100× by checkpointing and no further.

## Decision

**Accept the cost, state it plainly, and treat the dispute rate as a first-class parameter
rather than an afterthought.**

Nothing here is fixable by tuning instrumentation, so the three consequences below are what
this ADR is actually for.

### 1. The coordinator's cost claim is unaffected, and the distinction now matters

Cairn's central claim has always been that **the coordinator's** work is `O(log n)` messages
and one instruction, independent of execution length. That is still true and still measured.

What was never qualified is *whose* cost that is. Arbitration is cheap for the referee and
expensive for the two parties. Every document that said "arbitration is cheap" now says which.

### 2. The dispute rate has a budget, and it is tighter than it looks

At ~200× per party per dispute, a disputed fraction `d` of units adds roughly `400d` units of
work per unit delivered. Keeping that under 10% of total effort requires

> **`d` below about 1 in 4,000.**

That is achievable — a dispute only opens when two executions of the same unit disagree, which
requires a liar or broken hardware — but it is a *requirement*, and it was not written down
anywhere before. It also means canary sampling and reputation are load-bearing for **cost**,
not only for confidence: their job is to keep bad workers out of the assignment stream before
they generate disputes.

### 3. It reopens the alternative ADR-0005 rejected, as a contingency with a computable threshold

ADR-0005 rejected materializing machine state into linear memory — a shadow stack for the
operand stack and locals, the way Arbitrum's WAVM and Optimism's Cannon do it — on the grounds
that it taxes *all* execution to serve the rare case.

That trade can now be priced. If a shadow stack costs some factor `s` on every unit but lets a
party produce a trace on their own JIT, it beats the current design when `s < 400d`. At
`s ≈ 1.5` (a plausible +150% for turning every local access into a load and store), the
crossover is around **`d ≈ 0.4%`**.

So: below a dispute rate of roughly one in 250, the current design wins, and it wins by a wide
margin at the rates we expect. Above it, the rejected alternative is better. **That is a
contingency with a number attached rather than a closed door**, and if the dispute rate is ever
observed above a few tenths of a percent, this is the thing to reach for.

## Consequences

**Metering optimisation drops in priority.** It now buys 18%–41% on the interpreted dispute
path and nothing at all on the honest path. It is still worth doing eventually — it is a small,
well-understood change — but it is no longer the highest-value work, and the good-first-issues
list says so. **ADR-0007's reinstatement of it stands; its urgency does not.**

**ADR-0007's other conclusion is untouched and is the important one.** The honest path costs
**0%** on a real optimising compiler. That is what a volunteer pays, and it is the number the
project lives or dies by.

**Checkpointing in `Replay` becomes worth doing.** It halves the dispute cost, it is a
self-contained change to one party's bookkeeping, and it changes nothing about the protocol.

**Bench and docs now carry the ratio.** `docs/benchmarks.md` reports interpreter ÷ JIT per
workload, so this cost cannot quietly stop being visible.

## Alternatives considered

**Say nothing, since disputes are rare.** Rejected on the same grounds as everything else in
this repository: a cost nobody has written down is a cost nobody can budget for, and the
project's whole claim to be worth trusting is that it publishes its own bad numbers.

**Have the coordinator produce both traces instead.** It can — it is trusted and it has the
interpreter — but then a dispute costs the coordinator two full re-executions, which destroys
the one scaling property Cairn actually has. The point of the design is that the referee's work
does not grow with execution length.

**Ship the interpreter as the honest path so no engine change is ever needed.** That is a 100×
tax on every unit to avoid a 200× tax on one in several thousand. Not close.
