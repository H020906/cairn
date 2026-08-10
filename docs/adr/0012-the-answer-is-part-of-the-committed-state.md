# ADR-0012 — The answer is part of the committed state

- **Status:** Accepted
- **Date:** 2026-08-10
- **Completes:** [ADR-0011](0011-a-volunteer-that-cannot-argue-is-not-challenged.md)

## Context

[ADR-0011](0011-a-volunteer-that-cannot-argue-is-not-challenged.md) built the interactive dispute
protocol and, in doing so, found something it could only note rather than fix:

> Bisection convicts liars, not the merely wrong. Both parties replay identical bytes under the
> same deterministic interpreter, so an honest party's replay reproduces the truth whatever its
> own engine did earlier. A party whose original answer was wrong for non-adversarial reasons —
> a miscompiled build, faulty memory — replays honestly, agrees at every step, and bisection
> reports `NoDisagreement`.

That case then fell back to the referee **re-executing the unit**, at a full interpreted
execution. It was the most expensive path the system had, and it was reached in the *common*
case rather than the adversarial one — bad RAM is more ordinary in volunteer computing than
fraud.

The reason it was expensive is a gap in the commitment, and stating it plainly is uncomfortable:

**The state commitment did not cover the answer.** It covered memory, globals, the operand stack,
the call stack, the dropped segments, the program counter and the fuel — everything about *how*
the machine ran, and nothing about *what it said*. So two parties could agree on every root of a
million-instruction execution and the coordinator would have proved nothing at all about the one
thing it dispatched the unit to learn.

Bisection was right to report no disagreement. There was no disagreement *in what it was looking
at*.

## Decision

`StateCommitment` gains an eighth field: `output`, the digest of the bytes written through
`cairn.output` so far.

```text
root = H( domain ‖ memory_root ‖ globals ‖ operand_stack ‖ call_stack
          ‖ segments ‖ output ‖ pc ‖ fuel )
```

**A digest, not the bytes.** `cairn.output` *replaces* rather than appends, so no instruction
ever reads the buffer back and an adjudicator never needs its contents — only a commitment to
them. A witness therefore carries 32 bytes whether the workload answered four bytes or a
megabyte, which is what keeps a witness small and adjudication independent of the answer's size.

**The consequence, and the point of the whole change:** an agreed trace now *determines* the
answer. When bisection reports `NoDisagreement`, the coordinator asks one party for the final
state, checks that witness against the root **both parties already committed to**, and compares
two hashes. It executes nothing — not the unit, and unlike a conviction, not even one
instruction.

## Consequences

Measured end to end against the release binaries, one honest volunteer and one with
`--wrong-answer` (a broken engine: wrong result, honest replay):

```json
{"state":"settled","by":"bisection","rounds":4,"messages":4,
 "verdict":"nobody lied — both parties' replays agreed, and the trace they agreed on says
            the second party reported the wrong answer (4 messages, nothing executed)",
 "output":"bd3e5cfce4250000"}
```

Four messages, on a 1,050,030-instruction unit that previously cost a full interpreted
re-execution. The dispute did not even need to bisect: the parties agreed at step 0 and at the
end, which is where `resolve` stops.

**Every state root in the system changed.** That is a protocol break, and it is handled as one:
`wire.rs`'s format version goes to 2, and a version-1 witness is **refused rather than read**. A
permissive decoder would produce a witness that reconstructs a *different* root, which arrives at
adjudication as "this witness does not match the agreed state" — indistinguishable from a party
fabricating one. The version check is what makes that an upgrade notice instead of an
unexplained conviction.

**It cost one line of hashing per commit and no measurable time.** The digest is maintained when
the answer is written rather than recomputed at each commit, so committing stays independent of
the answer's size.

**A latent bug surfaced, and it is the more valuable half of this ADR.** Asking a party for a
witness of the **final** state was something nothing had ever done. It failed. `Machine::commit`
reported a finished machine's program counter as the module's entry point, while
`Witness::commitment` reported zero — because a witness does not know the entry point. For any
module whose entry index is not zero the two commitment paths disagreed about the final state, so
a party supplying a perfectly good witness had it refused **as though it had fabricated one**.

That is this project's worst failure shape — an honest volunteer convicted, silently, in a rare
path — and it had been sitting behind the fact that no test asked for a witness at the last step.
Both paths now use one constant, and the regression test walks `0..=total` across modules with
three different entry indices. The `=` is the whole test.

**What this does not fix.** A unit accepted after a single execution is still accepted on trust;
committing the answer only helps once there are two parties to compare. Canaries and reputation
remain the mechanism for the single-execution case, and remain unbuilt.

## Alternatives considered

**Carry the answer's bytes in the witness.** Simpler, and it makes an adjudicator able to read
the answer rather than only verify it. Rejected: a witness would grow with the answer, so
arbitration would stop being independent of the workload's size — the property
[ADR-0001](0001-verification-by-dispute-not-replication.md) rests on. Nothing reads the buffer
back, so nothing needs it.

**Leave the answer out and keep re-executing.** Honest, and what ADR-0011 shipped. Rejected once
it was clear the expensive path was the *common* one: bisection catches liars, and liars are the
rare case.

**Hash the answer at every commit instead of maintaining a digest.** One less field to keep in
step. Rejected because `commit()` runs at every snapshot boundary and at every bisection
question, so it would make the cost of committing scale with the answer.

**Have the coordinator ask both parties for the answer and take the majority.** Two parties, no
majority. And it would be trusting the parties about the very thing they disagree on, which is
what a commitment exists to avoid.
