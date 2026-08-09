# Cairn cost benchmark

**Instruction counts are exact, reproducible, and machine-independent. Wall-clock figures are not**, and this document measures how much they are not rather than asserting an error bar — see *Noise floor*. Any wall-clock figure smaller than its workload's noise is printed as *not resolved* instead of as a result.

Times are the fastest of 15 interleaved runs on one machine. Unless a section says otherwise they use Cairn's own interpreter, which is the **slow** path — the one that only runs during arbitration. *On a JIT rather than the interpreter* measures the same things under wasmtime, and the two do not agree at all about what metering costs.

## Instruction count

How many more instructions the instrumented module executes.

| workload | bare | metered | full | metering | canonicalization |
|---|---:|---:|---:|---:|---:|
| integer loop | 30000013 | 38000021 | 38000021 | 1.27× | 1.00× |
| float kernel | 11500015 | 13500023 | 28500029 | 1.17× | 2.11× |
| memory sweep | 6291602 | 8388842 | 8388842 | 1.33× | 1.00× |
| recursion | 7309646 | 11123374 | 11123374 | 1.52× | 1.00× |

## Wall-clock, decomposed

Each column is the cost of adding one thing to the one before it. `s` in ADR-0001's formula is the last column.

| workload | bare | +metering | +snapshots | +canonicalization | **s** |
|---|---:|---:|---:|---:|---:|
| integer loop | 257.6ms | 1.26× | 1.02× | 0.99× | **+27%** |
| float kernel | 119.8ms | 1.12× | 0.96× | 2.80× | **+201%** |
| memory sweep | 76.1ms | 1.39× | 1.21× | 0.79× | **+32%** |
| recursion | 201.2ms | 1.22× | 1.05× | 0.95× | **+23%** |

The three middle columns are shown for decomposition only. Read them against the noise floor below before drawing anything from them — on at least one workload here the harness cannot tell these apart from nothing.

## Noise floor

Measured, not assumed: these rows compare two configurations that produced identical module bytes, so every difference shown is the harness. Nothing in this document smaller than the largest of them is a result.

| workload | identical bytes timed twice |
|---|---:|
| integer loop | +1.7% |
| float kernel | — (canonicalization changes this module) |
| memory sweep | -3.8% |
| recursion | +4.6% |

**Error bar: ±5%.**

## On a JIT rather than the interpreter

wasmtime, compiling through Cranelift. Compilation and instantiation are outside the timer; only the call to `cairn_run` is measured. This is the closest available look at what a volunteer's own engine would pay — every other figure in this document is the interpreter.

| workload | honest path, JIT | honest path, interpreter | **interpreter ÷ JIT** | full instrumentation, JIT |
|---|---:|---:|---:|---:|
| integer loop | 2.5ms | 262.0ms | **104×** | +491% |
| float kernel | 831.4µs | 118.0ms | **142×** | +503% |
| memory sweep | 1.9ms | 73.2ms | **38×** | +181% |
| recursion | 1.4ms | 210.5ms | **150×** | +472% |

**The `interpreter ÷ JIT` column is the one that prices a dispute.** A trace commitment covers the operand stack and every frame's locals, which no host engine exposes ([ADR-0005](adr/0005-the-fast-path-cannot-snapshot.md)), so a challenged party cannot produce one on the engine they ran the work with — they must re-execute under Cairn's interpreter. That ratio, not the instrumentation overhead, is what a dispute actually costs them.

The rightmost column is metering's cost on a compiler, and it is included because it is startling and because it is **not a cost anyone pays**: nothing runs the fully instrumented module on a JIT. See [ADR-0008](adr/0008-a-dispute-costs-an-interpreted-re-execution.md).

## Two metering encodings, two engines

Both charge the same amounts at the same points; only the encoding differs. `HostCall` is two instructions and a host call per basic block, `Global` is four instructions into an exported counter and no call. Canonicalization is off in every column so that nothing else moves.

| workload | instructions: none / host call / global | interpreter: host call | global | **global ÷ host call** | JIT: host call | global | **global ÷ host call** |
|---|---|---:|---:|---:|---:|---:|---:|
| integer loop | 30000013 / 38000021 / 46000029 | 343.8ms | 416.6ms | **1.21×** | 16.1ms | 2.7ms | **0.17×** |
| float kernel | 11500015 / 13500023 / 15500031 | 115.2ms | 145.0ms | **1.26×** | 6.8ms | 2.0ms | **0.30×** |
| memory sweep | 6291602 / 8388842 / 10486082 | 239.4ms | 277.8ms | **1.16×** | 10.2ms | 3.5ms | **0.35×** |
| recursion | 7309646 / 11123374 / 14937102 | 552.3ms | 603.1ms | **1.09×** | 17.0ms | 2.8ms | **0.16×** |

Read the two ratio columns against each other. A host call is cheap next to interpreted arithmetic and ruinous next to compiled arithmetic, so the encoding that wins depends entirely on who is running the module — and under [ADR-0005](adr/0005-the-fast-path-cannot-snapshot.md) the metered module is run by Cairn's interpreter and nothing else. See [ADR-0009](adr/0009-metering-through-a-global-the-engines-disagree.md).

What metering costs a compiler, against the same module unmetered:

| workload | host call | global |
|---|---:|---:|
| integer loop | +540% | +8% |
| float kernel | +520% | +84% |
| memory sweep | +252% | +22% |
| recursion | +563% | +7% |

This is the number that decides whether an engine Cairn does not control could ever report how much work it did.

## The two paths, after ADR-0005

The fast path cannot snapshot, so it runs the determinism-only module and returns a result; the fully instrumented module runs only when a result is disputed. The left column is what every honest worker pays. The right column is what a disputed unit costs, on top of an execution that already happened.

| workload | honest path (ADR-0006) | honest, canonicalizing everywhere | disputed re-execution |
|---|---:|---:|---:|
| integer loop | **≈0% (±2%)** | +3% | +27% |
| float kernel | **-2%** | +165% | +201% |
| memory sweep | **≈0% (±4%)** | +9% | +32% |
| recursion | **≈0% (±5%)** | ≈0% (±5%) | +23% |

The middle column is what the honest path cost before ADR-0006 narrowed canonicalization to the few operations that can actually leak a NaN payload. Exact instruction counts against bare, which is where that change is unambiguous: **integer loop 1.00×** (was 1.00×), **float kernel 1.00×** (was 2.30×), **memory sweep 1.00×** (was 1.00×), **recursion 1.00×** (was 1.00×).

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
| 2^10 | 6144 | 3.21× |
| 2^12 | 1536 | 1.63× |
| 2^14 | 384 | 1.30× |
| 2^16 | 96 | 1.16× |
| 2^18 | 24 | 1.18× |
| 2^20 | 6 | 1.13× |

## Dispute cost against execution length

The claim ADR-0001 rests on: arbitration does not get more expensive as the disputed execution gets longer.

| diverge | execution length | bisection rounds | log2(length) | party's time, replaying from 0 | with checkpoints |
|---|---:|---:|---:|---:|---:|
| early (step 4) | 19028 | 14 | 14 | 9.0ms | **7.9ms** (1.1×) |
| early (step 4) | 190028 | 18 | 18 | 24.6ms | **20.0ms** (1.2×) |
| early (step 4) | 1900028 | 21 | 21 | 264.8ms | **153.4ms** (1.7×) |
| late (step 19016) | 19028 | 15 | 14 | 23.5ms | **6.3ms** (3.7×) |
| late (step 190016) | 190028 | 17 | 18 | 221.5ms | **23.8ms** (9.3×) |
| late (step 1900016) | 1900028 | 21 | 21 | 2.8s | **173.5ms** (16.0×) |

The rounds column is the coordinator's cost and does not grow with length. The two time columns are one **party's**, which is a different and much larger thing — see [ADR-0008](adr/0008-a-dispute-costs-an-interpreted-re-execution.md). Checkpointing is invisible to the protocol: the verdict is asserted identical with and without it.

**What a dispute costs a party depends on where the divergence is, not on how long the execution was.** An early divergence is answered by replaying a few instructions `log₂(n)` times and was never expensive; checkpoints buy it nothing, which is why they are recorded while answering rather than laid down in advance. A late divergence is where the naive `O(n log n)` bites, and where resuming from a checkpoint earns its memory.

## Witness size

An adjudicator's cost is set by this, not by the disputed execution's length. Pages are 64 KiB each and dominate; everything else is tens of values.

**The worst case below is a property of this workload, not a bound.** None of these use `memory.fill`, which reaches as far in one instruction as its length says — a 100,000-byte fill touches two pages, and a longer one touches more. ADR-0001 says so in prose; `tests/exact_costs.rs` pins it with a number.

| measure | value |
|---|---:|
| instructions sampled | 20000 |
| most pages one instruction needed | 1 |
| mean pages per instruction | 0.062 |
| deepest operand stack | 2 |
| worst-case witness payload | 64 KiB |

## Against ADR-0001

`s` is the honest path's overhead, which after [ADR-0005](adr/0005-the-fast-path-cannot-snapshot.md) is determinism instrumentation alone. It ranges from **-4%** to **+5%** across these four shapes. Full instrumentation, which now runs only on a disputed unit, costs up to +201%.

| scheme | cost multiplier |
|---|---:|
| BOINC, N = 2 | 2.00× |
| Cairn, best case | 1.09× |
| Cairn, worst case | 1.18× |

Using the canary rate (0.03) and replication rate (0.1) ADR-0001 assumed. Those two are policy, not measurements — they are chosen, and choosing them differently moves these numbers.

**Verdict: ADR-0001 holds** at these settings — worst case 1.18× against 2.00×.
