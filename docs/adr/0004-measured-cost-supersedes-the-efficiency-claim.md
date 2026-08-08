# ADR-0004 — What verification actually costs, and what that does to ADR-0001

- **Status:** Accepted
- **Date:** 2026-08-08
- **Supersedes:** the cost analysis in [ADR-0001](0001-verification-by-dispute-not-replication.md) §"Expected cost". The rest of ADR-0001 stands.

## Context

ADR-0001 argued that Cairn beats replication on cost. Its arithmetic was:

| Scheme | Cost multiplier |
|---|---|
| BOINC, N = 2 | ≈ 2.00× |
| Cairn | ≈ 1 + s + c + r ≈ **1.18×** |

with `s` (instrumentation overhead) assumed at ≈ 0.05, `c` (canary rate) at 0.03 and `r`
(selective replication) at 0.10. That ADR labelled these design targets and said explicitly
that establishing the real curve was a deliverable rather than an assumption.

`c` and `r` are policy dials — they are chosen, not discovered. **`s` is the only one of the
three that is a fact about the code**, and it has now been measured. `runtime/benches/cost.rs`
produces the figures below; `docs/benchmarks.md` is its output.

## What was measured

Four workload shapes, each executed under four configurations, on Cairn's own interpreter.
Wall-clock is the fastest of seven runs; instruction counts are exact.

| Workload | metering | snapshots | canonicalization | **measured `s`** |
|---|---:|---:|---:|---:|
| integer loop | 3.16× | 0.95× | 1.00× | **+201%** |
| float kernel | 1.15× | 1.01× | 2.28× | **+167%** |
| memory sweep | 1.28× | 1.11× | 1.01× | **+43%** |
| recursion | 1.21× | 0.99× | 0.94× | **+13%** |

Each column is the cost of adding one thing on top of the previous one.

**These wall-clock figures are noisy to about ±10%, and the table says so itself**: three of
the ratios are below 1.00×, meaning adding work apparently made execution faster, which is
impossible. Re-running moves the per-workload `s` values by several points. The instruction
counts in `docs/benchmarks.md` are exact and stable; the times are not.

That resolution is far too coarse to argue about a few percent — and far finer than it needs
to be for the conclusion here, because the gap between assumption and measurement is an order
of magnitude, not a few points.

## Decision

**ADR-0001's cost claim is withdrawn. Cairn must not be described as cheaper than
replication.**

Measured `s` is **0.13 to 2.01**, against the 0.05 assumed — wrong by roughly three to forty
times. Putting the measured values back into the same formula:

| Scheme | Cost multiplier |
|---|---:|
| BOINC, N = 2 | 2.00× |
| Cairn, best measured case | 1.26× |
| Cairn, worst measured case | 3.14× |

So Cairn is cheaper than replication for the recursion and memory-sweep shapes, and **more
expensive** for the integer and floating-point ones. Floating point is the shape the project
exists to serve, which makes that the worst place for it to lose.

## Where the cost goes

The decomposition matters more than the total, because the three sources behave differently.

**Fuel metering dominates block-dense code.** The integer loop's instruction count rose only
1.27×, but its time rose 3.16× — each injected `charge` costs several times an average
instruction. It is a call, and a call is the most expensive thing the interpreter dispatches.
The gap between 1.27× instructions and 3.16× time is the whole finding: the cost is not the
instructions added, it is *which* instruction is added.

**Canonicalization dominates floating-point code**, and there the cost tracks the instruction
count closely (2.11× instructions, 2.28× time). Six instructions after every arithmetic
operation is simply what the sequence costs.

**Snapshots are modest and tunable.** At the default `2^16` interval they cost between nothing
measurable and 1.11×. Raising the interval to `2^20` roughly halves what remains, at the price
of coarser pre-committed brackets and more replay for a disputing worker.

## What this does not touch

The arbitration mechanism itself measured exactly as designed, and those claims stand:

- **Dispute cost is independent of execution length.** Bisection rounds against trace length:
  21,022 steps → 15 rounds; 210,022 → 18; 2,100,022 → 21. A hundredfold longer execution costs
  six more rounds, which is `log₂` and nothing else.
- **Witnesses are small.** Across 20,000 sampled instructions, the most any one instruction
  needed was **one 64 KiB page**, and the mean was 0.062 pages. Operand stacks stayed two deep.

ADR-0001 was right about the mechanism and wrong about the price.

## Caveats that cut both ways

These numbers come from **Cairn's interpreter, which is the slow path**. Production's fast
path is the browser's own JIT, and the two terms would move in opposite directions there:

- **Metering would likely get worse.** A call across the JIT-to-host boundary costs relatively
  more than a call inside an interpreter that dispatches everything anyway.
- **Canonicalization would likely get much better.** The sequence is a compare and a
  never-taken branch. A JIT compiles that to a couple of machine instructions; the interpreter
  pays six full dispatches for it.

So the measured `s` is neither an upper nor a lower bound on the production figure. **It is
the only number that exists**, and stating it is better than continuing to quote an assumption.
Measuring on the fast path requires the browser worker, which is out of scope.

## What could change the answer

One concrete change is worth recording, because it targets the largest measured term and is
implementable without touching the protocol:

**Replace the `charge` call with a global counter.** Instead of
`i32.const N; call $charge`, emit `global.get $fuel; i32.const N; i32.add; global.set $fuel`
plus a threshold test that calls out only when a snapshot is actually due. That turns the
common path from a call into three arithmetic instructions. On the integer loop — where
metering costs 2.51× — this is where the overhead lives.

This is not done. It is the first thing to try if someone wants ADR-0001's conclusion back.

## Consequences

- README, ARCHITECTURE and ADR-0001 no longer state a cost advantage. What they state instead
  is the measured range and the fact that it is workload-dependent.
- **The project's honest pitch narrows**: Cairn makes verification *cheap to arbitrate* and
  *cheap to join*, and its overhead is competitive with replication for some workloads and
  worse for others. That is a smaller claim than ADR-0001 made and it is one the code supports.
- `docs/benchmarks.md` is regenerated by `cargo bench`. It should be regenerated, and this ADR
  revisited, after any change to the instrumentation pass or the interpreter's dispatch.
