# ADR-0015 — Canaries are what catch a cheat, and they are grounded in replication

- **Status:** Accepted
- **Date:** 2026-08-10
- **Completes and corrects in part:** [ADR-0001](0001-verification-by-dispute-not-replication.md)

## Context

ADR-0001's cost model is `1 + s + c + r`. `s` was measured (ADR-0004, ADR-0006), `r` was a dial
with a number in it, and **`c` did not exist**. Until this change nothing in Cairn ever caught a
dishonest volunteer *by itself*: a wrong answer was found only when a second volunteer happened to
disagree with it, which happens on the sampled fraction of units and never on the rest. On the
other 90%, a cheat was accepted after one execution and nobody was any the wiser.

`grid.rs` carried a warning against fixing this badly:

> Reputation is what decides *who* gets replicated, and inventing a scoring rule with no real
> workers to score would be fiction.

That warning is right, and it shapes what is built here. ADR-0001 asks for "a per-worker posterior
on *returns correct results*"; the standard estimator for a run of pass/fail observations is the
Beta-Binomial mean, so that is what this computes and no scoring rule is invented. What *is*
invented — the thresholds — is exposed as dials with stated defaults, exactly as `--replicate` is,
and their effect is measured rather than asserted.

## Decision

**A canary is a decided unit, queued again under a fresh index, whose answer the coordinator
already knows.** It is dispatched from the same path as real work, at a rate that depends on the
volunteer's standing. Answering it wrongly is direct evidence of a wrong answer: no second
volunteer, no dispute, no re-execution.

**Reputation is counts plus a policy.** `Record` holds what was observed — canaries passed and
failed, disputes lied in and abandoned, results accepted — and `Policy` holds what it means, so
changing the policy never means rewriting history. Everything is integer arithmetic in permille;
`float_cmp` is denied in this workspace and there is no reason to introduce a float where a ratio
of two counts is exact.

Three things are kept apart on purpose:

- **A failed canary or a proven lie is not evidence to be weighed**, it is an answer the
  coordinator *knows* was wrong. `is_proven_wrong` is separate from the posterior, so a thousand
  subsequent right answers do not un-do one known-wrong one.
- **Silence costs far less than a lie.** ADR-0001 insists on this and it is one of the few places
  where a wrong default punishes honest people: a volunteer that stopped answering may have closed
  a laptop. Default weights: a proven lie 20, silence 1.
- **Accepted results are contribution, not trust.** Almost every unit is accepted after a single
  execution, so counting them as evidence of honesty would mean a cheat earning trust by cheating.

**The number nine is not a choice.** With a `Beta(1,1)` prior and a 900‰ threshold, the posterior
after `p` clean canaries is `(1+p)/(2+p)`, which first clears 900‰ at `p = 9`. The
`proving_canaries` floor is therefore non-binding at the defaults, and is kept for the operator
who lowers the threshold — at 600‰ a single canary reaches 667‰, and one honest answer would buy
the reduced sampling rate a cheat wants.

**The schedule is unpredictable; the content is not.** Which unit is a canary comes from
`blake3(per-run secret ‖ worker ‖ lease number)`, not from a counter. The counter is the idiom
`--replicate` uses, and here it would let a volunteer that counts its own leases know exactly
which unit is the checked one and answer that one honestly. The secret is per-process and is
never written down: what must survive a restart is *which canaries were issued and how they came
out*, not the coin that chose them.

### The finding: a canary is only as true as the unit it was copied from

This came from a test that failed, and it is the sharpest thing in the change.

A unit accepted after a **single** execution carries one volunteer's word for its answer — which
is the entire point of the project. Minting a canary from one of those takes a cheat's wrong
answer and promotes it to *the answer the coordinator knows*. The next volunteer handed that
canary is then marked as a cheat **for being right**. The mechanism would not merely fail to catch
cheats; it would launder them into convicting honest people, which is this project's worst
outcome arrived at through its newest feature.

So a canary source must be **corroborated**: either two volunteers answered it and agreed, or
every volunteer who answered it was already trusted — and trusted means it passed canaries drawn
from units corroborated the first way. The chain is grounded.

**That means `c` and `r` are not independent, and ADR-0001's `1 + s + c + r` is wrong to add them
as if they were.** Corroboration comes from replication; trust comes from canaries. With
`--replicate 0` there are no corroborated units, so no canaries are minted, so no volunteer ever
becomes trusted and none is ever checked. **Replication is what canaries are grounded in**, and a
grid that turns it off has turned them off too. The coordinator now says so on startup.

### Selective *sampling*, not selective replication

ADR-0001 asks for "selective replication — new workers, low-reputation workers and high-value
units still get a second execution". What is built is per-worker **canary rate** instead: 30‰ once
trusted (ADR-0001's `c ≈ 0.03`, the figure its cost model was written around), 250‰ until then.

Two reasons. A unit's quorum is fixed when it is queued, deliberately, so that it cannot change
under a worker's feet — making it depend on who took it would undo that. And a canary produces
*definitive* evidence where replication produces only a disagreement that still has to be settled.

## Consequences

Measured against the real `Grid` — real minting, real reputation, the real dispatch path — with
volunteers that return wrong answers at a known rate. Thirty volunteers per row, medians, policy
at its defaults:

| cheat rate | units until caught | wrong answers accepted | never caught in 900 |
|---:|---:|---:|---:|
| 1000‰ | 3 | 3 | 0 of 30 |
| 500‰ | 4 | 2 | 0 of 30 |
| 200‰ | 15 | 3 | 0 of 30 |
| 100‰ | 21 | 2 | 0 of 30 |
| 50‰ | 102 | 5 | 2 of 30 |
| 10‰ | ≥900 | ≥9 | **19 of 30** |

Volunteers still uncaught at 900 units are counted *as* 900, so the last two rows are lower
bounds — the truth for 10‰ is worse than what is printed.

**The shape is the finding, and it is not flattering. Sampling bounds the damage a cheat does; it
does not bound the time it takes.** The "wrong answers accepted" column stays around two to five
across two orders of magnitude of cheat rate, because a cheat that halves its rate roughly doubles
its lifetime. A volunteer corrupting one unit in a hundred is very likely still out there after
nine hundred units.

That is the honest read of `c`, and it should temper how ADR-0001's model is quoted: canaries make
cheating *unprofitable per unit*, not *impossible*. A deployment that needs a hard bound on wrong
answers reaching the science needs replication on the units it cares about, not a higher `c`.

What this costs, and what is still missing:

- ~~**Replication catches cheats and reputation never hears about it.**~~ **Closed by
  [ADR-0017](0017-a-verdict-nobody-can-read-is-not-a-verdict.md).** The re-execution route reported
  its verdict as prose, so there was no structured "this party was wrong" for a record to be made
  from. `by_re_execution` now returns a typed verdict and the grid charges the volunteer it names —
  as a wrong answer, never as a lie, for the reason that ADR spends most of its length on.
- **Dispute-derived reputation does not survive a restart — still true of *bisection*.**
  ADR-0017 closed half of this: canary outcomes were already journalled and re-execution verdicts
  now are too. A conviction is not, because it is reached in a sweep that has no journal in scope.
- **Collusion defeats this**, as ADR-0001 says. Two volunteers who share inputs and answers will
  recognise a canary, because one of them produced the source unit's answer.
- **A cheat that is caught is not punished.** It is checked harder, and that is all. Cairn has no
  penalties, and deciding to exclude a volunteer is a policy with consequences for real people —
  it needs an operator, not a constant.

## Alternatives considered

**A canary corpus computed by the coordinator.** Answers it knows because it worked them out
itself. Rejected: the coordinator would be executing units, which is the cost the whole project
exists to avoid, and a separate corpus drifts away from the live input distribution — which
ADR-0001 names as exactly what makes canaries distinguishable.

**Reusing the source unit's index.** Simpler, and it announces every canary: `/api/status` is
public and says which units are decided.

**A counter-based schedule**, as `--replicate` uses. Rejected above: a worker can count its own
leases.

**Excluding a volunteer that fails a canary.** Tempting and out of scope. A failed canary can be
failing RAM as easily as fraud — ADR-0001 says so — and the difference matters to a person who
donated their machine. Sampling harder is a response the coordinator can make on its own;
exclusion is one an operator should make.

**Weighing a failed canary into the posterior and nothing more.** Rejected because a posterior
recovers: a cheat that fails one canary and then passes fifty is back above any threshold, and it
still returned an answer that was known to be wrong.
