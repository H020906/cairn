# ADR-0009 — Metering through a global: the two engines want opposite things

- **Status:** Accepted
- **Date:** 2026-08-09
- **Follows up:** [ADR-0007](0007-metering-is-a-jit-problem-not-an-interpreter-problem.md),
  [ADR-0008](0008-a-dispute-costs-an-interpreted-re-execution.md)

## Context

`canon.rs` charges fuel by injecting `i32.const N; call $charge` at the head of every basic
block. [ADR-0007](0007-metering-is-a-jit-problem-not-an-interpreter-problem.md) measured that
at 18%–41% in Cairn's interpreter and **+484% to +505%** under wasmtime, and proposed replacing
it with a module-local counter plus a threshold test — "three arithmetic instructions on the
common path, entering the host only when a snapshot is actually due."
[ADR-0008](0008-a-dispute-costs-an-interpreted-re-execution.md) then demoted the change, having
established that nothing runs the fully instrumented module on a JIT.

Two things about that plan turned out to be wrong before a line of it was written, and a third
turned out to be the reason to do it anyway.

**The threshold test does not exist.** WebAssembly has `local.tee` but no `global.tee`, so
reading a counter, adding to it, storing it back and comparing the result against a threshold
is eight instructions, not three: `global.get`, `i64.const`, `i64.add`, `global.set`,
`global.get`, `i64.const`, `i64.ge_u`, `if`/`end`. Four of those are the accumulation and four
are the test.

**The test is not needed.** It was there to decide when to enter the host, and the host was
being entered to enforce a ceiling and to schedule snapshots. But whoever executes the module
already knows the count — it is in a global they can read. Cairn's interpreter can intercept
the write, which is what it does. A host engine does not need to intercept anything, because
under [ADR-0005](0005-the-fast-path-cannot-snapshot.md) it is not producing a trace and its
ceiling is its own affair: wasmtime has fuel and epoch interruption, a browser can terminate a
Web Worker. Enforcement on the honest path is allowed to be imprecise, because a volunteer who
stops early has not produced a wrong answer — they have produced no answer.

So the encoding is **four instructions and no branch**:

```wat
global.get $cairn_fuel
i64.const N
i64.add
global.set $cairn_fuel
```

**And the third thing: this makes the honest path able to say how much work it did.** Under the
host-call encoding that was unavailable at any price a volunteer would accept. Under this one it
is an exported global — run the module, read the number. Nothing in Cairn consumes that yet; the
coordinator is unbuilt. But it is the difference between a network that can account for
contributed work and one that can only count completed units, and it costs four instructions.

## What it measures

Both encodings charge the same amounts at the same points, so they produce identical fuel
totals, identical exhaustion points and identical snapshot schedules — `tests/metering.rs` pins
all four, and the differential gate now runs every corpus case under both across all three
engines. What differs is the price.

`cargo bench`, error bar **±5%** measured rather than asserted. Canonicalization is off in every
column so nothing else moves.

| workload | instructions: none / call / global | interpreter | JIT |
|---|---|---:|---:|
| integer loop | 30,000,013 / 38,000,021 / 46,000,029 | 1.21× | **0.17×** |
| float kernel | 11,500,015 / 13,500,023 / 15,500,031 | 1.26× | **0.30×** |
| memory sweep | 6,291,602 / 8,388,842 / 10,486,082 | 1.16× | **0.35×** |
| recursion | 7,309,646 / 11,123,374 / 14,937,102 | 1.09× | **0.16×** |

Both ratios are global ÷ host call. **The same change is 9%–26% slower on one engine and three
to six times faster on the other**, and the instruction counts say why: the global encoding
executes strictly *more* instructions — four per charge site against two — and simply does not
make a call. Interpreted, two dispatches plus an intercepted call beat four dispatches. Compiled,
four arithmetic instructions are nearly free and the call is a wall.

Against the same module *unmetered*, which is the figure that decides whether an engine Cairn
does not control could ever report its own work:

| workload | host call | global |
|---|---:|---:|
| integer loop | +540% | **+8%** |
| float kernel | +520% | **+84%** |
| memory sweep | +252% | **+22%** |
| recursion | +563% | **+7%** |

Metering on a compiler goes from *unthinkable* to *a rounding error on three workloads out of
four*. The float kernel's +84% is the outlier and the shape is instructive: its basic blocks are
short, so it charges often relative to the work it does. Charge density, not workload size, is
what this encoding is sensitive to.

## Decision

**Build both encodings, ship neither as a new default, and write down which engine wants which.**

`canon::Config` gains `meter: Metering` in place of `meter_fuel: bool`. The two shipped
configurations are unchanged:

- `Config::dispute_path()` keeps `Metering::HostCall`. Cairn's interpreter is the only engine
  that ever executes a metered module ([ADR-0005](0005-the-fast-path-cannot-snapshot.md),
  [ADR-0008](0008-a-dispute-costs-an-interpreted-re-execution.md)), and the global encoding is
  **slower there.** Issue 6b existed to make a dispute cheaper; measured, this change makes it
  dearer. That is the answer, and it is the answer only because the question was asked with a
  benchmark rather than an intuition.
- `Config::honest_path()` keeps `Metering::Off`. The global encoding would let the honest path
  report its own fuel — the new capability, and the interesting one — but **nothing consumes
  that number yet.** The coordinator is unbuilt. A cost paid on every unit needs a consumer
  before it is paid.

So this ADR ships a *facility* and a *measurement*, not a change of behaviour. That is deliberate
and it is the part most likely to be misread later: the encoding is not dead code kept "in case",
it is the answer to a question the coordinator will ask on its first day — *how do we know how
much work a volunteer did?* — recorded now, while the measurement that justifies it is fresh.

## Consequences

**Metering on a compiler stops being ruinous, and that reopens a door ADR-0007 closed.** The
+484%–+505% figure was never a cost anyone paid, but it was a cost that made a whole class of
design impossible: anything requiring the fast path to count. That class is now open. Nothing in
Cairn needs it today.

**`cairn_fuel` joins `cairn.charge` as a reserved name.** A submitted module exporting it is
rejected — whatever kind it names, not only a global. The reason to refuse all kinds rather than
just the matching one is that the pass appends its own export unconditionally, and two exports
sharing a name is not a valid module; a submitted module claiming the name would therefore fail
at *instrumentation* time with a much worse message than the gate's.

**The counter is machine state, and it is state that must not drift.** It is an ordinary mutable
global, so `StateCommitment.globals` hashes it automatically — and `StateCommitment.fuel` records
the same number a second time. The redundancy is harmless but the *agreement* is not optional:
the interpreter's meter is what decides exhaustion and snapshots, while the global is what gets
hashed, so a discrepancy would put two honest workers' roots at odds for the same execution.
`Machine::charge_to` keeps them equal on every exit, including the exhausted one, where the
refused charge is wound back out of the global.

**The counter is appended past the module's own globals, which is why nothing needs remapping.**
Unlike the `charge` import — inserted among the function indices, shifting every defined function
by one — a validated module's global index space is untouched. This is load-bearing and quiet:
the day something inserts a global instead of appending one, every `global.get` in every workload
in the network means something different.

**The differential gate got stronger in a way worth naming.** Every corpus case now runs under
both encodings across all three engines, and under the global one the two reference engines
report an instruction count **they were not told**. Under the host-call encoding the harness
accumulates the total itself, one call at a time, which proves little more than that the calls
happened. Here the module keeps its own count and the engine hands the global back, so agreement
means the engine executed the same basic blocks in the same order — and wasmtime reaching Cairn's
exact total is the closest this repository comes to evidence that a volunteer's own engine could
report its work honestly.

**Two numbers to keep an eye on.** `tests/exact_costs.rs` pins the instruction counts under the
new encoding and, more usefully, pins the *relation*: the gap between the two encodings must
equal the gap between the host-call encoding and no metering at all, since one injects two
instructions per site and the other four. If that relation breaks, the two encodings have stopped
charging at the same places, and every claim on this page stops being true at once.

## Alternatives considered

**The threshold test as ADR-0007 described it.** Eight instructions instead of four, to enter
the host at a point the host does not need to be told about. It would be necessary if the
metered module ran somewhere that could not read the global at all — but reading an exported
global is the one thing every WebAssembly embedding can do.

**A 32-bit counter.** Half the width, and on a 32-bit target the addition is one instruction
rather than a pair. Rejected because fuel is a `u64` everywhere else and a counter that wraps
after four billion instructions would wrap *silently*, producing a smaller number rather than an
error — and the workloads this project exists for run for far longer than that.

**Let the interpreter count its own steps and inject nothing.** Tempting, and very nearly
right: `Machine` already maintains a step counter for free, and after ADR-0005 it is the only
engine that executes a metered module. It was not taken because the count would then be a
property of *the interpreter* rather than of *the module*, and the whole architecture of this
project is that determinism lives in the binary where every engine can see it. The moment the
count lives only in our code, a second implementation of Cairn's interpreter — which is a thing
an open protocol should be able to have — has nothing to agree with.
