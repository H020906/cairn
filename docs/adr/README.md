# Architecture Decision Records

Decisions that are expensive to reverse, recorded with the reasoning that produced them.

An ADR is written when a choice closes off alternatives that a future maintainer might
otherwise assume are still open. Routine choices do not need one; the test is whether
someone six months from now would look at the code and reasonably ask *"why on earth is it
done this way?"*

## Index

| # | Title | Status |
|---|---|---|
| [0001](0001-verification-by-dispute-not-replication.md) | Verify by dispute, not by replication | Accepted |
| [0002](0002-language-boundaries.md) | Language boundaries: Java for coordination, Rust for execution, no third | Accepted |
| [0003](0003-determinism-constraints.md) | Enforcing bit-exact determinism by instrumenting the workload binary | Accepted |
| [0004](0004-measured-cost-supersedes-the-efficiency-claim.md) | What verification actually costs, and what that does to ADR-0001 | Accepted |

## Reading order

Start with **0001** — it is the reason the project exists, and 0002 and 0003 are both
consequences of it. 0003 in particular describes the constraint that makes 0001 sound; if
you are looking for the part most likely to be subtly wrong, it is there.

Then read **0004**, which measured 0001's cost argument and refuted it. 0001 is left intact
with a correction banner rather than edited, so the original reasoning and the evidence
against it can both be read. That is what superseding is for.

## Format

```md
# ADR-NNNN — Title in the imperative

- **Status:** Proposed | Accepted | Superseded by ADR-XXXX
- **Date:** YYYY-MM-DD

## Context
What forces are in play. What makes this a real decision rather than an obvious one.

## Decision
What we are doing. Stated so a reader can act on it.

## Consequences
What this buys, what it costs, and what it now constrains. Include the bad parts.

## Alternatives considered
What was rejected and why. A rejected option with no stated reason will be re-proposed.
```

Supersede rather than edit. A decision that turned out wrong is more useful with its
history intact than quietly rewritten.
