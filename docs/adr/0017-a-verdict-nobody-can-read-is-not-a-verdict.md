# ADR-0017 — Give the re-execution route a typed verdict

- **Status:** Accepted
- **Date:** 2026-08-12

## Context

Cairn settles a disagreement in one of three ways, and [ADR-0011](0011-a-volunteer-that-cannot-argue-is-not-challenged.md)
explains why the third exists: a browser volunteer cannot answer a bisection challenge, because
answering means producing a state root and no host engine exposes the state one covers
([ADR-0005](0005-the-fast-path-cannot-snapshot.md)). Challenging such a volunteer would time it out
and convict it for silence. So when neither party can argue, the referee executes the unit itself
and compares.

That route ends holding something valuable: **the unit's true answer.** Comparing it against the
two submissions identifies a volunteer whose result is wrong, with the same certainty a canary
gives and without either party having to argue. It is the only mechanism in Cairn besides a canary
that catches a wrong answer outright.

`dispute::by_re_execution` returned that finding as a `String`.

```rust
(false, true) => format!("the {} was wrong", Party::First),
```

The sentence reached the unit's outcome, the JSON at `/api/status`, and a line in the journal. It
reached [`reputation`](../../coordinator/src/reputation.rs) nowhere, because there was nothing in it
to read. [ADR-0015](0015-canaries-are-what-catch-a-cheat.md) shipped with this listed first among
its open items — *"replication catches cheats and reputation never hears about it"* — and named the
fix.

**The gap was not cosmetic.** `Standing::ProvenWrong` changes how often a volunteer is checked, from
30‰ to 250‰ under the default policy. A volunteer that the coordinator had *executed the unit to
disprove* went on being sampled as if nothing had happened.

## Decision

**`by_re_execution` returns a `ReExecution`, and the grid records what it says.**

```rust
pub enum ReExecution {
    Refuted { wrong: Party },
    BothRefuted,
    NoAnswer,
    NotADisagreement,
}
```

Four consequences follow from that one change.

**A refutation is a wrong answer, never a lie.** `Record` gains a `refuted` counter beside
`failed` and `lied`, and it weighs exactly what a failed canary weighs. The referee proved the
*result* wrong and proved nothing about intent — this route exists precisely because these parties
cannot argue, and a browser volunteer whose engine diverges reaches it in perfect good faith.
Only losing a bisection shows a party corrupting its own replay, and only that is `lied`, at
twenty times the weight. **Collapsing the two would put this project's worst failure — an honest
volunteer convicted — one careless edit away.**

**No new weight dial.** [ADR-0015](0015-canaries-are-what-catch-a-cheat.md) says the weights are
the one genuine judgement in that file and that there is precisely one of them. Re-execution leaves
the coordinator in the same position a canary does — holding the true answer — so the two are
weighed alike. Inventing a dial to make them differ would have been a belief with nothing behind
it. The counter is separate anyway, so that the units-until-caught measurement in
`tests/canaries.rs` is not contaminated by catches no canary made, and so an operator can see
which mechanism did the catching.

**The interactive route charges too.** `Conclusion::FellBack` now carries a `ReExecution` rather
than prose, so a bisection that could not settle an argument and fell back to re-execution records
whatever the re-execution established. The comment that used to sit there — *"nothing is proven
about anybody"* — was true only because the verdict was a string nothing could read.
`ReExecution::NoAnswer` is the case that really does establish nothing, and it says so in the type.

**It survives a restart.** `Entry::Settled` carries the verdict and the refuted worker *names*, and
replay charges them again. The names are recorded rather than re-derived because
[ADR-0014](0014-the-coordinator-keeps-a-log-not-a-database.md) requires replay to read facts rather
than re-make decisions: turning a `Party` back into a worker means knowing which submission arrived
first, which is a decision, and guessing it wrong would charge the volunteer that was right.

## Consequences

**What it buys.** The second-most-powerful evidence Cairn can gather now reaches the mechanism that
acts on evidence. A volunteer refuted by re-execution is `ProvenWrong`, is checked at the untrusted
rate from then on, and stays that way across a restart.

**What it costs.** A wire-format change. The verdict byte sits where a length-prefixed sentence used
to, so a journal written before this ADR will not load. `verdict_from_tag` returns `None` for
anything it does not recognise and the reader turns that into a corrupt-file error, because
restoring a coordinator with an invented verdict is worse than refusing to start. There is no
migration; the journal is a local file for a coordinator nobody is yet running in anger.

**What is still missing, and it is now the largest gap of its kind.** A **bisection** verdict still
does not survive a restart. Conviction happens in `Grid::account_for_finished_disputes`, which is a
sweep over concluded arguments rather than a request path, and it has no journal in scope. So a
coordinator restarted after convicting a liar has forgotten the conviction, while one restarted
after refuting a wrong answer has not. That asymmetry is not principled — it is where the journal
happens to be reachable from — and it is the contained job this ADR hands to whoever takes it.

**What it does not change.** There are still no penalties. A refuted volunteer is checked harder and
nothing else. Excluding a volunteer is a policy with consequences for real people and it needs an
operator, not a constant.

## Alternatives considered

**Parse the sentence.** Match on `"the second party"` at the call site. Rejected for the reason the
API already gives for exposing `verdict_fields` alongside its prose: anything branching on English
breaks the first time the wording improves — and two of this project's own tests were doing exactly
that, which is how the shape of this bug stayed invisible.

**Count a refutation as a lie.** Simpler, one counter fewer, and defensible on the grounds that the
coordinator *proved* the result wrong. Rejected: it proves the result wrong and says nothing about
why. The volunteers on this route are the ones that cannot argue, which is to say browsers, which
is to say the volunteers this project is for. Charging them twenty failed canaries for an engine
divergence is the failure mode every other decision here is arranged to avoid.

**Credit the party that was right as a passed canary.** Tempting and symmetric — the referee
confirmed their answer against its own execution, which is what a canary does. Rejected as a
larger judgement than this change needs: it would make winning a disagreement a way to earn trust,
and the collusion question that opens deserves its own argument rather than riding along here.
They are credited with `accepted` — work done, not trust earned — exactly as on the ordinary path.

**Write a separate journal entry for the refutation.** Mirrors `Entry::Canary`, which records a
worker and an outcome and nothing else. Rejected because the fact belongs to a settlement that was
already being written down, and two entries that must agree are a way for them to disagree.
