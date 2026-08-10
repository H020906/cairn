# ADR-0006 — Canonicalize NaNs where they can escape, not where they are made

- **Status:** Accepted
- **Date:** 2026-08-09
- **Builds on:** [ADR-0003](0003-determinism-constraints.md), [ADR-0005](0005-the-fast-path-cannot-snapshot.md)
- **Restores:** the cost conclusion of [ADR-0001](0001-verification-by-dispute-not-replication.md), by a different route than ADR-0001 argued

## Context

[ADR-0005](0005-the-fast-path-cannot-snapshot.md) moved fuel metering and snapshots off the
honest path, leaving one instrumentation cost behind: **NaN canonicalization**. It was not a
small remainder. On the float benchmark it executed **2.30× the bare instruction count** and
cost about **+150%** in time — the difference between Cairn beating replication and losing to
it on exactly the workload shape the project exists to serve.

It also could not simply be deferred the way metering was. WebAssembly leaves the payload bits
of a *computed* NaN to the engine, so two honest workers on different engines can hold
different bits for the same value. Canonicalization is what makes them agree, and agreement is
the precondition for everything else.

The question is not whether to canonicalize. It is **where**.

The pass canonicalized after every operation that can mint an engine-chosen NaN: `add`, `sub`,
`mul`, `div`, `min`, `max`, `sqrt`, the four rounding operations, `demote` and `promote`. In a
numeric kernel that is nearly every instruction, and it fires whether or not a NaN ever occurs.

## Decision

**On the honest path, canonicalize immediately before the operations that can turn a payload
difference into an answer difference. Leave every other float value alone.**

The observation is that an engine-specific NaN is only a problem if it can *become* something
other than a NaN. Nearly nothing lets it:

| What a NaN meets | What comes out | Payload matters? |
|---|---|---|
| `add`, `mul`, `div`, `sqrt`, `min`, `max`, rounding | a NaN | No |
| `abs`, `neg` | a NaN | No |
| any comparison (`eq`, `lt`, `ge`, …) | `0`, for every payload | No |
| `br_if` on such a comparison | the same branch on every engine | No |
| `trunc` | a trap, for every payload | No |
| `trunc_sat` | `0`, for every payload | No |
| `local.set` / `local.get`, operand stack | the same NaN | **Not on the honest path** |
| **store** | those bytes in memory, which is what `cairn.output` reports | **Yes** |
| **`global.set`** on a float global | those bits, kept | **Yes** |
| **`reinterpret`** | the payload, as an integer | **Yes** |
| **`copysign`** | an ordinary number carrying the NaN's **sign** | **Yes** |

So the escape set is four entries, and `canon::escape_site` is that table in code.

The row about locals and the operand stack is the one ADR-0005 unlocked. A state commitment
hashes both, so under the old model any NaN anywhere in the machine had to be canonical. The
honest path no longer commits to machine state — only to a result — so a non-canonical NaN
sitting in a local is invisible.

### `copysign` is the entry that is easy to miss

Every other row is about payload bits. `copysign(x, y)` takes the *sign* of `y`, and the sign
of a computed NaN is as unspecified as its payload. `copysign(1.0, sqrt(-1.0))` could be
`+1.0` on one engine and `-1.0` on another, with no NaN in the answer at all.

This is not hypothetical, and it is not merely argued. Both engines here return
`0xfff8_0000_0000_0000` from `sqrt` of a negative number — a canonical NaN **with the sign bit
set**. And deleting this one entry from the escape set makes the randomised differential fail
on its third case with **`-1.5` against `+1.5`**: no NaN anywhere in the answer, just a sign
flipped by reading the sign bit of a NaN two engines disagreed about. Canonicalizing the top
operand clears that bit and fixes the result.

### `Everywhere` canonicalizes at escapes too

Not redundancy — it is what keeps the two modules the same program.

A workload can build a NaN with a payload it chose, via `f64.reinterpret_i64` of a constant or
by loading bytes it stored, without ever executing a NaN-producing operation. That value is
the program's own deterministic data and `Everywhere` correctly leaves it alone. If `AtEscapes`
canonicalized it on the way out and `Everywhere` did not, the two modules would compute
different answers for that program — and observational equivalence between them is precisely
what ADR-0005 rests on.

Canonicalizing at escapes under **both** settings makes every value that becomes observable
canonical under either, whatever happened upstream. The two site sets are disjoint, so this
costs the dispute path a handful of extra sequences and nothing else.

## What it measures

Exact instruction counts, honest path against bare — machine-independent, and the number that
matters:

| workload | before | after |
|---|---:|---:|
| float kernel | 2.30× | **1.00×** |
| integer loop, memory sweep, recursion | 1.00× | 1.00× |

Wall-clock, honest path against bare, on a run whose measured noise floor was ±2%: every
workload including the float kernel now lands inside that noise. Back into ADR-0001's formula:

| scheme | cost multiplier |
|---|---:|
| BOINC, N = 2 | 2.00× |
| Cairn, best case | **1.12×** |
| Cairn, worst case | **1.15×** |

**ADR-0001's conclusion is restored** — though not its reasoning. ADR-0001 got 1.18× by
assuming instrumentation cost ≈5% on a path that also produced a trace commitment. That path
does not exist ([ADR-0005](0005-the-fast-path-cannot-snapshot.md)) and that overhead was never
5% ([ADR-0004](0004-measured-cost-supersedes-the-efficiency-claim.md)). The number came back
because the honest path now does *almost nothing*, not because the original estimate was right.

## Consequences

**Enabling.** Floating-point workloads — molecular docking, the first intended target — no
longer pay a determinism tax proportional to their arithmetic.

**Enabling.** `canon::escape_site` is a short, auditable table rather than a dataflow analysis.
The alternative under consideration was proving statically that an operation cannot produce a
NaN; that is a real analysis with a real chance of being subtly wrong, and it turned out to be
unnecessary.

**Limiting — a wrong escape set is a consensus bug, not a performance bug.** A missing entry
means two honest workers disagree and the protocol convicts one of them. That risk is why the
list is deliberately conservative and why it is tested adversarially rather than by inspection:
`nan_payloads_cannot_escape` drags a deliberately non-canonical NaN through arithmetic,
comparisons, branches, globals, both float widths and both reinterpret directions, and requires
all of it to agree across two engines and two instrumentation settings.

**The test was checked for teeth, which matters more than the test passing.** Deleting
`I64ReinterpretF64` from the escape set makes it fail on the leaked payload — and makes
`float_arithmetic_agrees` fail as well, on `sqrt` of a negative. A version of that test using
`0.0/0.0` would have proved nothing: both engines return the canonical pattern for it, so
canonicalizing and not canonicalizing produce identical bytes.

**Confirmed later against the engine this is actually about, and the confirmation is not
theoretical** (2026-08-10). When the browser volunteer's own engine was added to the
differential gate, the same teeth check was run against it: delete `F64Copysign` from
`escape_site`, and **V8 disagrees with Cairn's interpreter immediately** — `+1.5` against
`-1.5`, on the third generated float expression, under the honest configuration.

That is worth stating plainly, because this ADR argued for the `copysign` entry from the
specification rather than from evidence and called it "the one most easily missed":

> The *sign* of a computed NaN is as unspecified as its payload.

It is unspecified, and **V8 exercises the freedom in the opposite direction from Cairn's
interpreter.** So this entry is not defensive programming against a hypothetical engine. Without
it, every volunteer running a browser would eventually produce a different answer from every
volunteer running Cairn's interpreter, be disputed, and lose — convicted of cheating for the
offence of running in a browser. See `runtime/tests/differential.rs`.

**Limiting — this is honest-path only, and deliberately.** The dispute path still canonicalizes
after every NaN-producing operation, because arbitration compares machine states and a
non-canonical NaN in a local would make two honest workers' commitments differ. Metering and
snapshots are already dispute-only; canonicalization is now the third thing on that list.

**Watch item.** SIMD is rejected today. If it is ever admitted, every lane-wise float operation
joins the escape analysis and this table has to be revisited before that happens, not after.

## Alternatives considered

**Prove statically that a NaN cannot occur, and delete the check.** The plan of record before
this ADR. A "cannot be NaN" lattice handles conversions from integers and NaN-propagating
operations like `min`, but `add`, `sub` and `mul` need *finiteness* rather than non-NaN-ness
(`inf - inf` is a NaN), and finiteness is not preserved by those operations, so an accumulator
in a loop degrades to unknown on the first back-edge. It would have been substantial work for
little benefit on exactly the shape that needed it.

**Canonicalize on read instead of on write.** Symmetric and worse: loads are far more common
than the four escape operations, and a load of program-written bytes needs no canonicalization
at all.

**Emit a branchless sequence instead of `if`/`else`.** A `select`-based form is roughly the
same instruction count and would likely help on a JIT while helping little in the interpreter.
Orthogonal to this ADR and still available — but it optimises a sequence that now runs
approximately never on the honest path.

**Do nothing and accept the cost.** Rejected: it was the difference between 1.15× and 2.63×
against replication's 2.00×, on the workload shape the project was built for.
