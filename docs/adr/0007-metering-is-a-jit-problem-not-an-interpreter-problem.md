# ADR-0007 — Metering is a compiler problem, and the interpreter was hiding it

- **Status:** Accepted, **with its headline figure reinterpreted below**
- **Date:** 2026-08-09
- **Confirms and reinstates:** the remediation [ADR-0004](0004-measured-cost-supersedes-the-efficiency-claim.md) proposed and [ADR-0005](0005-the-fast-path-cannot-snapshot.md) withdrew
- **Corrected in part by:** [ADR-0008](0008-a-dispute-costs-an-interpreted-re-execution.md)

> **The +505% below is real, and nobody pays it.** This ADR measured fuel metering on a JIT
> without first asking who runs the fully instrumented module — and the answer is that no host
> engine ever does, for the same reason ADR-0005 gives: a trace commitment covers the operand
> stack and every frame's locals, which no JIT exposes. A challenged party re-executes under
> **Cairn's interpreter**, where metering costs 18%–41%.
>
> The cost that dominates a dispute is the change of engine: the interpreter is **37×–142×**
> slower than the JIT. See [ADR-0008](0008-a-dispute-costs-an-interpreted-re-execution.md).
>
> **This ADR's other conclusion is untouched, and it is the important one: the honest path
> costs 0% on a real optimising compiler.** The reinstated metering fix is still correct, just
> no longer urgent.

## Context

Every wall-clock figure in this project came from Cairn's own interpreter, which is the slow
path — the one that only executes while arbitrating a dispute. What a volunteer actually runs
is a compiler. ADR-0004 said so, and refused to call its numbers an upper or a lower bound for
that reason. It also made a specific guess:

> **Metering would likely get worse.** A call across the JIT-to-host boundary costs relatively
> more when the surrounding code is compiled.

That guess is now measured, by running the same four workloads under **wasmtime**, compiling
through Cranelift. It was right, and by a much larger margin than "likely".

## What was measured

Overhead against the same workload with no instrumentation, on wasmtime, timing only the call
to `cairn_run` — compilation and instantiation are outside the timer:

| workload | honest path | full instrumentation |
|---|---:|---:|
| integer loop | −0% | **+502%** |
| float kernel | −0% | **+496%** |
| memory sweep | +1% | **+154%** |
| recursion | +0% | **+484%** |

The interpreter reports **+18% to +41%** for that same right-hand column. The two engines
disagree by more than an order of magnitude about the cost of the identical transformation.

## Decision

**Two conclusions, and they point in opposite directions.**

### 1. The honest path is free, and that is now established rather than argued

Zero percent, on a real optimising compiler, on all four shapes including floating point. This
is the strongest evidence the project has for
[ADR-0005](0005-the-fast-path-cannot-snapshot.md) and
[ADR-0006](0006-canonicalize-nans-at-escapes-on-the-honest-path.md) together: moving trace
production to dispute time and narrowing canonicalization to escape sites did not merely reduce
the honest path's cost, it removed it.

The interpreter could never have shown this. Its own noise floor is ±2%, and its bare execution
is slow enough that a few extra instructions disappear into it. The JIT makes bare execution
fast, which is exactly what makes any remaining overhead visible — and there is none.

### 2. Fuel metering, which is now dispute-only, costs five to six times on a compiler

`canon.rs` charges fuel by injecting `i32.const N; call $charge` at every basic block. In an
interpreter a host call is another match arm, costing about what any other instruction costs.
In compiled code the surrounding instructions are near-free and the call is not: it goes
through a trampoline, spills live registers, and blocks optimisation across the call site.
Roughly 8M injected calls in the integer loop turn a tight compiled loop into something six
times slower.

**So ADR-0004's proposed fix is reinstated** — replace the call with a module-local global
counter plus a threshold test, so the common path is three arithmetic instructions and the host
is only entered when a snapshot is actually due. ADR-0005 withdrew that recommendation on the
grounds that metering had left the honest path. That reasoning was right about *where* the cost
falls and wrong to conclude the cost stopped mattering. See the consequences below.

## Consequences

**This changes what a dispute costs, not what a work unit costs.** A volunteer running honest
work pays nothing. A volunteer who is challenged re-executes at roughly 6× — for one unit,
once. Whether that matters is a policy question about dispute rates rather than a throughput
question, and it is *bounded*: a party who finds it too expensive can decline and take the
defendant's penalty, which is by design the worse outcome for them.

**It does make one existing dial much more important.** The snapshot interval `k` was described
in ADR-0005 as no longer trading against honest-path speed, which is true. It now trades
against *dispute* cost, and dispute cost is 6× rather than 1.2×. Anyone tuning `k` should be
looking at the JIT column, not the interpreter one.

**The measurement is wasmtime, not a browser.** V8 and SpiderMonkey have different host-call
machinery and will not reproduce these numbers exactly. What transfers is the shape of the
result — a host call is expensive relative to compiled arithmetic and cheap relative to
interpreted arithmetic — and that shape is a property of compilation, not of Cranelift.

**Every earlier interpreter-only figure should now be read with this in mind.** They were not
wrong; they were measured on the path they said they were measured on. But "instrumentation
costs 20%" was an artefact of the interpreter being slow enough to absorb it. The document
keeps both columns rather than replacing one with the other, because the gap between them is
itself the finding.

## Alternatives considered

**Leave it, since it is dispute-only.** Tempting, and it is what ADR-0005 concluded. Rejected
now that the number is known: 6× is large enough that a coordinator could reasonably hesitate
to open disputes, and a verification mechanism nobody wants to invoke is not a verification
mechanism.

**Measure on a browser engine instead.** Better evidence, and not available without the browser
worker, which does not exist. wasmtime is a real optimising compiler and answers the question
that was open — whether compiled execution changes the conclusion — which speculation could
not.

**Reduce the number of charge sites instead of their cost.** Charging per basic block is
already the coarse option; going coarser means the fuel count stops being exact, and an exact
instruction count is what makes an execution addressable for bisection. The cost to attack is
the per-site cost, not the site count.
