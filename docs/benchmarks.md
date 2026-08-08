# Cairn cost benchmark

Wall-clock figures are the fastest of 7 runs on one machine, using Cairn's own interpreter. Instruction counts are exact and machine-independent. Ratios transfer between machines; absolute times do not.

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
| integer loop | 332.1ms | 3.16× | 0.95× | 1.00× | **+201%** |
| float kernel | 301.3ms | 1.15× | 1.01× | 2.28× | **+167%** |
| memory sweep | 196.5ms | 1.28× | 1.11× | 1.01× | **+43%** |
| recursion | 488.2ms | 1.21× | 0.99× | 0.94× | **+13%** |

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
| 2^10 | 6144 | 3.12× |
| 2^12 | 1536 | 1.65× |
| 2^14 | 384 | 1.26× |
| 2^16 | 96 | 1.18× |
| 2^18 | 24 | 1.19× |
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

Measured `s` ranges from **+13%** to **+201%** across these four shapes.

| scheme | cost multiplier |
|---|---:|
| BOINC, N = 2 | 2.00× |
| Cairn, best case | 1.26× |
| Cairn, worst case | 3.14× |

Using the canary rate (0.03) and replication rate (0.1) ADR-0001 assumed. Those two are policy, not measurements — they are chosen, and choosing them differently moves these numbers.

**Verdict: ADR-0001 DOES NOT HOLD** at these settings — worst case 3.14× against 2.00×.
