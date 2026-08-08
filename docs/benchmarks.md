test result: ok. 0 passed; 0 failed; 184 ignored; 0 measured; 0 filtered out; finished in 0.00s

# Cairn cost benchmark

**Instruction counts are exact, reproducible, and machine-independent. Wall-clock figures are not**, and this document measures how much they are not rather than asserting an error bar — see *Noise floor*. Any wall-clock figure smaller than its workload's noise is printed as *not resolved* instead of as a result.

Times are the fastest of 15 interleaved runs on one machine, using Cairn's own interpreter — which is the **slow** path. The fast path in production is a JIT that does not exist yet; see [ADR-0005](adr/0005-the-fast-path-cannot-snapshot.md) for what it can and cannot do.

## Instruction count

How many more instructions the instrumented module executes.

| workload | bare | metered | full | metering | canonicalization |
|---|---:|---:|---:|---:|---:|
| integer loop | 30000013 | 38000021 | 38000021 | 1.27× | 1.00× |
| float kernel | 11500015 | 13500023 | 28500023 | 1.17× | 2.11× |
| memory sweep | 6291602 | 8388842 | 8388842 | 1.33× | 1.00× |
| recursion | 7309646 | 11123374 | 11123374 | 1.52× | 1.00× |

## Wall-clock, decomposed

Each column is the cost of adding one thing to the one before it. `s` in ADR-0001's formula is the last column.

| workload | bare | +metering | +snapshots | +canonicalization | **s** |
|---|---:|---:|---:|---:|---:|
| integer loop | 322.3ms | 1.13× | 1.55× | 1.70× | **not resolved** |
| float kernel | 275.9ms | 1.16× | 1.02× | 2.23× | **+163%** |
| memory sweep | 211.7ms | 1.11× | 1.15× | 1.02× | **+30%** |
| recursion | 480.0ms | 1.20× | 1.14× | 0.89× | **+21%** |

The three middle columns are shown for decomposition only. Read them against the noise floor below before drawing anything from them — on at least one workload here the harness cannot tell these apart from nothing.

## Noise floor

Measured, not assumed: these rows compare two configurations that produced identical module bytes, so every difference shown is the harness. Nothing in this document smaller than the largest of them is a result.

| workload | identical bytes timed twice |
|---|---:|
| integer loop | +149.6% |
| float kernel | — (canonicalization changes this module) |
| memory sweep | -3.9% |
| recursion | +9.9% |

**Error bar: ±150%.**

## The two paths, after ADR-0005

The fast path cannot snapshot, so it runs the determinism-only module and returns a result; the fully instrumented module runs only when a result is disputed. The left column is what every honest worker pays. The right column is what a disputed unit costs, on top of an execution that already happened.

| workload | honest path (determinism only) | disputed re-execution (full) |
|---|---:|---:|
| integer loop | **not resolved** | not resolved |
| float kernel | **+147%** | +163% |
| memory sweep | **≈0% (±4%)** | +30% |
| recursion | **≈0% (±10%)** | +21% |

Instruction counts confirm the left column is canonicalization and nothing else: integer loop 1.00×, float kernel 2.30×, memory sweep 1.00×, recursion 1.00×.

## Snapshots taken at the default interval

| workload | snapshots | instructions per snapshot |
|---|---:|---:|
| integer loop | 457 | 83151 |
| float kernel | 175 | 77142 |
| memory sweep | 96 | 87383 |
| recursion | 106 | 104937 |

## Snapshot interval against cost

Lower `k` means finer pre-committed brackets for bisection and more hashing. This is the dial to turn if `s` is too high.

| interval | snapshots | cost vs no snapshots |
|---:|---:|---:|
| 2^10 | 6144 | 2.97× |
| 2^12 | 1536 | 1.60× |
| 2^14 | 384 | 1.22× |
| 2^16 | 96 | 1.19× |
| 2^18 | 24 | 1.14× |
| 2^20 | 6 | 1.16× |

## Dispute cost against execution length

The claim ADR-0001 rests on: arbitration does not get more expensive as the disputed execution gets longer.

| execution length | bisection rounds | log2(length) |
|---:|---:|---:|
| 21022 | 15 | 14 |
| 210022 | 18 | 18 |
| 2100022 | 21 | 21 |

## Witness size

An adjudicator's cost is set by this, not by the disputed execution's length. Pages are 64 KiB each and dominate; everything else is tens of values.

| measure | value |
|---|---:|
| instructions sampled | 20000 |
| most pages one instruction needed | 1 |
| mean pages per instruction | 0.062 |
| deepest operand stack | 2 |
| worst-case witness payload | 64 KiB |

## Against ADR-0001

`s` is the honest path's overhead, which after [ADR-0005](adr/0005-the-fast-path-cannot-snapshot.md) is determinism instrumentation alone. It ranges from **-4%** to **+147%** across these four shapes. Full instrumentation, which now runs only on a disputed unit, costs up to +163%.

| scheme | cost multiplier |
|---|---:|
| BOINC, N = 2 | 2.00× |
| Cairn, best case | 1.09× |
| Cairn, worst case | 2.60× |

Using the canary rate (0.03) and replication rate (0.1) ADR-0001 assumed. Those two are policy, not measurements — they are chosen, and choosing them differently moves these numbers.

**Excluded from this verdict: integer loop.** On that workload the harness's own error exceeded the effect being measured, so there is no number to include. Instruction counts for it are still exact and appear above.

**Verdict: ADR-0001 DOES NOT HOLD** at these settings — worst case 2.60× against 2.00×.
