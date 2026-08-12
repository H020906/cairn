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
| [0009](0009-metering-through-a-global-the-engines-disagree.md) | Metering through a global: the two engines want opposite things | Accepted |
| [0010](0010-the-referee-executes-so-the-coordinator-is-rust.md) | The referee executes, so the coordinator is Rust | Accepted, corrects 0002 in part |
| [0011](0011-a-volunteer-that-cannot-argue-is-not-challenged.md) | A volunteer that cannot argue is not challenged | Accepted, corrects 0010 in part |
| [0012](0012-the-answer-is-part-of-the-committed-state.md) | The answer is part of the committed state | Accepted, completes 0011 |
| [0013](0013-a-volunteer-computes-its-own-parallelism.md) | A volunteer computes its own parallelism, and reports under one name | Accepted |
| [0014](0014-the-coordinator-keeps-a-log-not-a-database.md) | The coordinator keeps a log, not a database | Accepted, amends 0002 |
| [0015](0015-canaries-are-what-catch-a-cheat.md) | Canaries are what catch a cheat, and they are grounded in replication | Accepted, completes 0001 |
| [0016](0016-math-belongs-in-the-module-not-the-host.md) | Math belongs in the module, not the host | Accepted, extends 0003 |
| [0017](0017-a-verdict-nobody-can-read-is-not-a-verdict.md) | Give the re-execution route a typed verdict | Accepted, closes an open item in 0015 |

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

Then **0010, 0011 and 0012** together, which are about the coordinator. 0010 found that the
referee *executes*, so it cannot be the Java service ADR-0002 planned. 0011 built the interactive
dispute protocol 0010 said was missing, and came back with four things that were only visible
from inside it — including that the fallback 0010 called a gap is not one. 0012 closes the
finding 0011 could only note: the commitment did not cover the workload's **answer**, so two
parties could agree on a million roots and prove nothing about the result. Fixing that made the
common, non-adversarial dispute cost four messages and no execution at all — and, on the way,
surfaced a latent bug that had been convicting honest parties in a path nothing had ever
exercised.

If you read only two of these, read 0005 and 0008 — the two places where the project asked
"but who is actually doing this?" and got an unwelcome answer. 0011's Finding 2 is the same
question asked a third time, about the volunteer this project is supposed to be for.

**0013** is the first one about the volunteer's own machine rather than about the protocol. It is
short and worth reading for two things: a setting that is dangerous because a plausible answer is
easy to give, and a scaling curve that bends at exactly the machine's performance-core count. A
sixteen-thread laptop donates about seven cores' worth of work, so counting reported cores
over-promises by nearly 2× — a fact about hardware that any distributed-computing project will
meet and few write down.

**0014** is the project deciding *against* its own architecture document. `ARCHITECTURE.md` says
SQLite; writing it found the coordinator has no queries to serve, so persistence is an append-only
log in the standard library instead. Read it for two things beyond the storage argument: what a
restart does to an argument somebody was in the middle of, and the finding that a lease is
**evidence** and a **reservation** at the same time — restoring only the first is what stops a
volunteer being punished for the coordinator crashing under it.

**0015** closes ADR-0001's last missing term. Read it for the measurement, which is the least
flattering number in the repository: **sampling bounds the damage a cheat does and not the time it
takes** — a volunteer corrupting one unit in a hundred is usually still undetected after nine
hundred units. It also corrects ADR-0001's cost model, which adds `c` and `r` as independent
terms: canaries must be copied from *corroborated* units, corroboration comes from replication, so
`r = 0` silently turns canaries off. That was found by a test which showed the naive version
laundering a cheat's answer into ground truth and then convicting honest volunteers for being
right.

**0016** asks where a workload's `exp` comes from and finds that the obvious answer — import it
from the host — would have made the grid convict honest volunteers on close to one `cbrt` call in
three. It belongs with 0003 and 0006 as the third instalment of the same argument: the first two
secured the arithmetic WebAssembly specifies, and this one covers the arithmetic it does not.

It also contains the most alarming single measurement in this repository, and it was not the one
being looked for. For the worst-case argument in the format, the platform's own `sin` returns
`-0.2227` where the answer is `1.0` — confirmed by exact integer arithmetic over a 3000-bit `pi`,
by V8, and by this project's library, all three agreeing against it. Host math is not only
inconsistent between hosts; it is not dependably correct on any of them.

Finally **0017**, which closes the first item on 0015's own list of what it left undone. The route
that settles a disagreement between two volunteers who cannot argue ends up holding the unit's true
answer, so it knows which of them is wrong — and it reported that as an English sentence, which
meant reputation never heard it. **A verdict nothing can read is not a verdict.** Read it for the
distinction it spends most of its length defending: being refuted is a wrong *answer*, and losing a
bisection is a wrong answer plus a party corrupting its own replay to defend it. The volunteers on
the re-execution route are the ones that cannot argue, which is to say browsers, which is to say
the volunteers this project is for — so charging them as liars would have been the failure every
other decision here is arranged to avoid.

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
