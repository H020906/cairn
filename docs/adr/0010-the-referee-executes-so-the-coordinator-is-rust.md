# ADR-0010 — The referee executes, so the coordinator is Rust

- **Status:** Accepted
- **Date:** 2026-08-10
- **Corrects in part:** [ADR-0002](0002-language-boundaries.md)
- **Corrected in part by:** [ADR-0011](0011-a-volunteer-that-cannot-argue-is-not-challenged.md)

> **Correction (2026-08-10, same day).** This ADR's language decision stands unchanged. Two of
> its remarks about the *dispute protocol* do not:
>
> 1. It called the re-execution fallback a **gap** awaiting a wire protocol. The wire protocol
>    now exists, and the fallback turned out to be a permanent, principled route rather than a
>    placeholder: a volunteer that cannot produce state roots — every browser, by
>    [ADR-0005](0005-the-fast-path-cannot-snapshot.md) — cannot be a party to a bisection, and
>    challenging one anyway would convict it for silence.
> 2. It repeated that closing the gap needed **"nothing in `runtime/`"**. Wrong: the disputed
>    state has to cross the wire and nothing could encode a `Witness`. `resolve` itself is
>    genuinely untouched, which was the half that mattered.
>
> See [ADR-0011](0011-a-volunteer-that-cannot-argue-is-not-challenged.md).

## Context

[ADR-0002](0002-language-boundaries.md) drew the language boundary on capability lines: Java 21
for coordination, Rust for execution, TypeScript for the web surface, and no Go. Most of that
still holds. One of its stated consequences does not:

> The domain model crosses exactly **one** language boundary (Java ↔ Rust), at exactly one
> place: the work-unit / result / trace-commitment wire format.

That sentence was written before the dispute protocol existed. Now that it does, the boundary
is in a different place, and it is not a wire format.

**The coordinator is the referee, and refereeing means executing an instruction.**

Splitting the protocol in half shows exactly where the line falls:

| | what it needs | Java? |
|---|---|---|
| **Bisection** — `dispute::resolve` | compare two hashes, move a bound | **yes.** It is a pure state machine over `u64` and 32-byte arrays. |
| **Adjudication** — `dispute::adjudicate` | rebuild a machine from a state witness and **execute one instruction** | **no.** |

Adjudication is the decisive act. Everything before it narrows the question; this is where the
answer comes from, and it is `Machine::restore(image, witness, …)` followed by `step()`. That
is the execution kernel, called from the coordinator.

## The three ways to keep the coordinator in Java, and why none is good

**Reimplement the interpreter in Java.** Out of the question, and it is worth stating why in
the strongest terms: it would create a **second implementation of consensus-critical code**.
Two implementations that disagree anywhere do not produce a bug report — they convict an honest
volunteer. Every differential test in this repository exists to prevent exactly this, and
deliberately introducing a second implementation to save a language boundary would be the
single worst decision available.

**Shell out to `cairn-worker` for each adjudication.** Workable. A dispute is rare, so a
process spawn is affordable. But it moves a type boundary to a process boundary, which is not
obviously simpler — the witness, the verdict and the image all have to be serialised anyway,
and the failure modes multiply.

**JNI into `cairn-runtime`.** Also workable, and genuinely the right answer for a production
system that wants Spring's persistence and transactional tooling. It is real work, and it is
work that buys nothing until there is a database to be transactional about.

## Decision

**The coordinator is Rust, in a new `coordinator/` crate, and ADR-0002's Java decision is
suspended rather than overturned.**

Suspended is the accurate word. The argument for Java was about persistence, transactions and
connection fan-in — the coordinator Cairn will eventually need. This is about the coordinator
Cairn can actually have: one that dispatches units, collects results, and settles the
disagreements, with no database behind it yet. In Rust that coordinator calls `dispute::resolve`
and `dispute::adjudicate` directly, and the language boundary disappears instead of moving.

Concretely, this also means:

- **One toolchain.** CI already builds Rust and the `wasm32` target; contributors already need
  cargo. Adding a JDK and Maven to run a demo coordinator is a real cost paid by everyone who
  clones the repository, for the benefit of nobody in the next two weeks.
- **`tiny_http` rather than a hand-rolled HTTP parser or an async stack.** Hand-rolling HTTP at
  a boundary that faces the internet would contradict the reason `validate.rs` is fuzzed.
  Pulling in `tokio` and `axum` for a blocking demo server would contradict every other
  dependency decision in this repository — see the note in `benches/cost.rs` about not adding
  `criterion`. `tiny_http` is blocking, thread-per-request, and small enough to read.

## Consequences

**ADR-0002's "exactly one narrow seam" consequence is withdrawn.** The seam it described was
the wire format; the real one is that the referee executes. Anyone re-reading ADR-0002 should
read that consequence as the thing this ADR corrects.

**The Java coordinator becomes a documented future, not a documented plan.** `ARCHITECTURE.md`
says so where it used to describe `server/`. The path back is clear and it has a trigger: when
the coordinator needs a database, transactions and reputation bookkeeping, JNI into
`cairn-runtime` for adjudication is the cost of moving, and it is a known cost rather than a
surprise.

**Rust's weakness here is real and should be named.** The coordinator is the part of Cairn
that most wants mature persistence tooling, and Rust's is thinner than Spring's. This decision
trades that away for a language boundary that would otherwise sit across the one operation
that must never be implemented twice.

**This is the fifth time in this repository that checking a premise before implementing
changed what got built.** The others are recorded in ADRs 0005, 0006, 0008 and 0009. The
pattern is worth naming because it keeps paying: *before building the thing the plan names,
confirm the plan still describes the system.*

## Alternatives considered

**Keep Java and accept the subprocess.** The strongest rejected option, and the one to reach
for if this decision is ever reversed under time pressure rather than on the merits.

**Write the coordinator in Go.** ADR-0002 refused a fourth toolchain and that refusal is
untouched — nothing here changes it.

**Do not build a coordinator at all; leave the workers as demonstrations.** Considered
seriously. Rejected because the gap between "two demos" and "one system" is the gap between a
project someone might use and a project someone can only read about, and it is smaller than it
looks: dispatch, collect, compare, dispute. The parts that are genuinely large — persistence,
reputation, canary policy — are explicitly *not* in this coordinator, and the code says so.
