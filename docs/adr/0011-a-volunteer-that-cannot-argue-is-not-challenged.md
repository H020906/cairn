# ADR-0011 — A volunteer that cannot argue is not challenged

- **Status:** Accepted
- **Date:** 2026-08-10
- **Corrects in part:** [ADR-0010](0010-the-referee-executes-so-the-coordinator-is-rust.md)

## Context

[ADR-0010](0010-the-referee-executes-so-the-coordinator-is-rust.md) shipped a coordinator that
settled every disagreement by **re-executing the unit itself**. It said so at length, in six
places, and called the missing piece a wire protocol:

> Bisection is an *interactive* protocol. It works by asking **the two parties** what state they
> claim at a given step, and they answer by re-executing on their own machines. There is no wire
> protocol for asking yet, so there is nobody to ask.

That protocol now exists. Building it produced two findings that were not visible from the
outside, and this ADR is about those rather than about the endpoints.

## Finding 1 — the witness has to cross the wire, so `runtime/` did change

The handover notes said twice, confidently:

> `dispute::resolve` is already generic over `Claimant`, so **nothing in `runtime/` changes**.

Half right. `resolve` is genuinely untouched — a [`Desk`] that blocks a referee thread until an
HTTP handler drops an answer in is enough to drive it across a network, which is the payoff of
having written it as a pure state machine.

But bisection does not *end* the dispute. It narrows it to one instruction, and then somebody
must **execute that instruction from the state immediately before it**. The coordinator does not
have that state, and it must not compute it: reaching step *n* costs `O(n)`, which is the entire
cost bisection exists to avoid. Computing it anyway would be the coordinator doing a party's work
while calling it arbitration.

So a **party** sends the state, as a `Witness`. And nothing in the repository could put a
`Witness` on a wire. `runtime/src/wire.rs` is the new part, and it is not a formality: the bytes
arrive from a party with a direct interest in the outcome, so its decoder is written to the same
standard as `validate.rs` — never panic, never allocate on a stranger's say-so, never hang. Every
length prefix is checked against the bytes that remain before anything is reserved, which bounds
the largest possible allocation by the input's own length and needs no magic constant.

Accepting a witness from an interested party is safe for one reason, and it is worth stating
exactly: `adjudicate` refuses any witness whose reconstructed commitment differs from the root
**both parties already committed to** during bisection. A fabricated witness cannot decide a
dispute. It can only fail to be accepted, at which point the other party is asked.

## Finding 2 — a volunteer must declare whether it can argue

Answering a challenge means producing the state root after *n* instructions. **No WebAssembly
engine outside this repository can do that** — the operand stack, a live frame's locals and the
frame chain are not things an embedder gets to read. That is [ADR-0005], and it is most of why
this project exists.

So the volunteer this project is *for* — a browser tab, no install, full speed — is structurally
incapable of being a party to a bisection. It is fast and blind.

Challenging one anyway does not fail gracefully. It times out, and a party that stops answering
**loses by default**. The result would be an honest volunteer convicted for running in a browser:
rare, silent, and concentrated on exactly the participants the project is trying to attract.

Therefore a submission carries `bisects`, the volunteer declares it, and the coordinator believes
it. The interactive route is taken only when **both** parties have declared. Otherwise the referee
re-executes the unit itself.

**The re-execution fallback is therefore permanent and principled, not a gap.** ADR-0010 treated
it as a placeholder. It is not one: it is what a network of blind volunteers is owed.

## Finding 3 — bisection convicts liars, not the merely wrong

Both parties answer by replaying **identical bytes on identical input under the same
deterministic interpreter**. An honest party's replay therefore reproduces the truth whatever its
own engine did earlier.

The consequence is sharper than it first looks. A party whose original answer was wrong for
non-adversarial reasons — a miscompiled build, faulty memory, a browser bug — replays *honestly*,
agrees with the other party at every step, and bisection reports `NoDisagreement`. Nobody lied, so
there is nobody to convict, and naming the wrong answer falls back to re-execution.

So: **bisection is not a way to decide which of two answers is right. It is a way to convict a
party that lies about its own execution.** Those coincide in the adversarial case, which is the
case [ADR-0001]'s economics are about — a rational cheater returning plausible answers is exactly
a party that must then lie about the trace. They do not coincide when the disagreement is an
accident.

A liar has to lie twice: once in the answer, and again in every root it claims afterwards. Lying
only once is not being caught cheaply — it is not cheating.

## Finding 4 — "not your turn" is not "nothing to do"

Found by running it, not by reasoning about it. The first end-to-end run against real binaries
ended with:

```
the second party stopped answering after 0 rounds and loses by default
```

The party had not stopped. The referee asks one side at a time, so a party spends most of a
dispute with nothing outstanding — and the polling worker, seeing an empty reply, counted that as
idleness and exited on its idle timeout. **It abandoned a dispute it was winning, and abandoning
means losing by default.** A volunteer would have been convicted for the *other* party being slow.

`/api/challenge` therefore answers three states, not two: a question, `{"waiting":true}`, or `204`.
The distinction is load-bearing, and it has a test named after it.

## Decision

1. `/api/challenge` (GET to collect a question, POST to answer it) is the interactive protocol.
   Questions are `length`, `root`, and `witness`; answers quote a **token**, and a stale token is
   refused so a slow party's reply cannot be counted as an answer to a question it was never
   asked.
2. `runtime/src/wire.rs` encodes a `Witness`, with a decoder that assumes the sender is hostile.
3. `Question`, `Answer` and the party-side `dispute::answer` live in `runtime/`, so there is
   **exactly one implementation of what an honest answer is**. A coordinator test double with its
   own idea of that would be a second implementation of consensus-critical code — the thing
   ADR-0010 called unthinkable.
4. A submission declares `bisects`. The interactive route requires both parties to have declared
   it; otherwise the referee re-executes. **The fallback is a route, not a gap.**
5. A party is served the **dispute-path** module, not the one it ran. They are different programs
   with different instruction counts, so "step 500,000" names a state only if both parties replay
   the same bytes. `/api/module/{id}?form=dispute`.
6. A party keeps **one warm `Replay` per dispute**. A fresh one per question discards the
   checkpoints and makes a dispute `O(n log n)` instead of `O(n)` — the mechanism in place, and
   none of it paid for.

## Consequences

Measured end to end against the release binaries, one coordinator and two native volunteers on
`sum-of-squares` with `input-a`:

| | |
|---|---:|
| instructions in the disputed execution | 1,050,030 |
| bisection rounds | 20 |
| questions and answers exchanged | 47 |
| instructions the coordinator executed | **1** |
| a party's slowest single answer | 29 ms |
| the witness, produced and encoded | 10.4 ms |

Verdict: *the second party lied about the instruction at step 499,999, found in 20 rounds of
bisection and proved by executing that one instruction.* The accepted output is the honest
party's, and **nobody re-ran the unit** — which is the whole claim, now visible in the running
system rather than only in `cargo run --example dispute`.

**What it costs.** A dispute is `2·log₂(n)` messages exchanged strictly one after another, so its
wall-clock time is dominated by polling latency rather than by computation — 20 rounds took about
20 seconds against roughly 100 ms of actual replay. That is fine for a rare event and it would
not be fine for a common one; it is another reason the dispute rate is the budget
[ADR-0008](0008-a-dispute-costs-an-interpreted-re-execution.md) says it is.

**What it does not do.** No penalties. `Conclusion` distinguishes a proven lie, an abandonment
and a fallback, and ADR-0001 wants those to cost a volunteer very differently — but acting on
that needs the reputation store that does not exist, so a verdict is recorded and read, not
applied.

## Alternatives considered

**Have the coordinator produce the witness itself.** It has the module and the input, so it could
execute to the divergence step and capture the state. That is `O(n)` — precisely what bisection
just spent 20 rounds avoiding. It would make the mechanism decorative.

**Replay both parties inside the coordinator and compare.** Rejected under ADR-0010 and still
rejected: it looks like the mechanism working while being the coordinator doing both parties'
work, which is the exact cost the design exists to avoid, dressed up as its avoidance.

**Assume every volunteer can argue.** This is the one that silently convicts browsers. See
Finding 2.

**Bind the output into the state commitment,** so that agreeing traces would prove agreeing
answers and the fallback could be cheap. Genuinely attractive, and out of scope here: the bytes a
workload writes through `cairn.output` are *determined* by the trace but *carried* by no single
root, so the coordinator must replay to learn them. Fixing that means an output buffer inside
`StateCommitment`, which touches `state.rs`, `machine.rs` and every differential test. Worth
doing; not worth doing as a side effect of this.

**A JSON parser in the worker.** The volunteer reads six fields out of documents this repository
also writes. `worker-native/src/client.rs` reads them with sixty lines and says loudly that it is
not a JSON parser — the same rule that kept `serde` out of the coordinator and `criterion` out of
the benchmarks: a dependency has to do something the standard library cannot. The mirror decision
is `api.rs` using `tiny_http`, because *that* side parses the open internet.

[ADR-0001]: 0001-verification-by-dispute-not-replication.md
[ADR-0005]: 0005-the-fast-path-cannot-snapshot.md
[`Desk`]: ../../coordinator/src/dispute.rs
