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
| [0004](0004-measured-cost-supersedes-the-efficiency-claim.md) | What verification actually costs, and what that does to ADR-0001 | Accepted, corrected by 0005 |
| [0005](0005-the-fast-path-cannot-snapshot.md) | The fast path cannot snapshot, so the trace moves to dispute time | Accepted |
| [0006](0006-canonicalize-nans-at-escapes-on-the-honest-path.md) | Canonicalize NaNs where they can escape, not where they are made | Accepted |
| [0007](0007-metering-is-a-jit-problem-not-an-interpreter-problem.md) | Metering is a compiler problem, and the interpreter was hiding it | Accepted, corrected by 0008 |
| [0008](0008-a-dispute-costs-an-interpreted-re-execution.md) | A dispute costs the parties an interpreted re-execution | Accepted |

## Reading order

Start with **0001** — it is the reason the project exists, and 0002 and 0003 are both
consequences of it. 0003 in particular describes the constraint that makes 0001 sound; if
you are looking for the part most likely to be subtly wrong, it is there.

Then read **0004**, which measured 0001's cost argument and refuted it, and **0005**, which
found that 0001's *execution model* could not be built at all and replaced it. 0001 and 0004
are left intact with correction banners rather than edited, so the original reasoning and the
evidence against it can both be read. That is what superseding is for.

Read **0004, 0005 and 0006** together, in that order — they are one argument told over three
documents, and reading any of them alone gives the wrong impression.

0004 is the project measuring its own headline claim and losing. 0005 is the project finding a
hole underneath the claim — the execution model could not be built — and, in the same pass,
discovering that 0004's instrument was less trustworthy than 0004 said. 0006 removes the cost
that was left, and ends with ADR-0001's conclusion restored at **1.15×** against replication's
2.00×.

Note what that sequence is *not*: it is not the original claim being vindicated. ADR-0001 got
its number by assuming an overhead that was never real, on a path that cannot exist. The number
came back because the honest path now does almost nothing. Getting the right answer for the
wrong reasons is still getting it wrong, and the record says so.

Then **0007**, which ran the same measurements on a compiler instead of an interpreter and
found the two disagree by an order of magnitude about what metering costs. It confirms the
honest path is genuinely free — 0% on a real optimising compiler — and it is the cleanest
example in the repository of why the engine you measure on is part of the measurement.

Then **0008**, one day later, which asked who actually runs the module 0007 had been timing and
found the answer was nobody. It corrects 0007's headline without touching its important half,
and replaces it with the number that does matter: a dispute costs each party an interpreted
re-execution, and the interpreter is 37×–142× slower than the JIT they ran the work on. That
turns the dispute *rate* into a budget with an actual figure in it.

If you read only two of these, read 0005 and 0008 — the two places where the project asked
"but who is actually doing this?" and got an unwelcome answer.

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
