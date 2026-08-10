# ADR-0013 — A volunteer computes its own parallelism, and reports under one name

- **Status:** Accepted
- **Date:** 2026-08-10

## Context

Until now a native volunteer ran one work unit at a time. On the machine this was developed on
that used one of sixteen hardware threads and left fifteen idle, which is a strange thing for a
project whose entire premise is that idle capacity is worth collecting.

Making it use the rest is not hard. Work units share nothing — each gets its own instance, its own
linear memory, and runs single-threaded with no locks — so there is nothing to synchronise and no
shared state to get wrong. The difficulty is entirely in two questions that look like tuning and
are not.

**How many at once?** The obvious answer is a `--jobs` flag, and it is the wrong one. A volunteer
runs programs somebody else wrote, on a computer somebody else owns and is usually still using. A
workload may declare up to `validate::Limits::max_memory_pages`, which is 256 MiB. A person with
32 hardware threads who types `--jobs 32` has asked for 8 GiB of their own laptop, and the failure
mode is not an error message — it is swapping, a machine that becomes unpleasant to use, and a
volunteer who uninstalls. The setting is dangerous *because* a plausible answer is easy to give.

**Under what name?** A machine that started sixteen workers would be sixteen volunteers as far as
the coordinator is concerned. It could then take both halves of a replicated unit, and the two
independent executions a quorum is supposed to buy would be one machine agreeing with itself.
`Grid::lease` already enforces one vote per *name*; nothing enforces one name per machine.

There is also a cost that was never in anybody's budget. A party to a dispute holds up to
`DEFAULT_CHECKPOINT_BUDGET` — 32 — clones of the machine, so that answering `log₂(n)` questions
costs `O(n)` rather than `O(n log n)`. **Arguing about a unit costs tens of times what running it
costs.** A volunteer that filled its memory with units and was then asked to defend one would run
out at the worst possible moment, and the worst outcome this project has is an honest volunteer
that loses an argument it should have won.

## Decision

**Parallelism is computed, and the operator's flags can only make it smaller.**

Three separate limits, in `worker-native/src/capacity.rs`:

```text
threads    = clamp(cores − 1, 1, 32)          lowered further by --jobs
concurrency = whatever a byte Allowance admits
checkpoints = min(32, budget ÷ 4 ÷ declared)   per workload
```

`cores − 1` leaves the machine usable by the person who owns it. The cap of 32 is **blast
radius**, not capacity: a volunteer holding *n* leases that loses power costs the grid *n*
reassignments, and beyond a few dozen a machine is better used as two volunteer processes — two
names, two failure domains, and each one still one vote per unit.

**Concurrency is bounded by memory, not by a count of threads.** A thread claims the bytes its
workload declared before executing and releases them afterwards, so the invariant is:

> the declared memory of all units executing at once never exceeds the budget.

This is what makes the count correct without re-planning when a grid serves several workloads: the
same fifteen threads run fifteen 64 MiB units or two 512 MiB ones, and nobody had to know in
advance which would show up. A unit larger than the whole budget runs *alone* rather than never.

**The budget is split rather than spent.** Half for units in flight; a quarter the dispute path
may draw on; a quarter left to the machine, which is the difference between donating and
surrendering. The checkpoint budget comes out of that quarter, and reaching zero is a legitimate
answer — `Replay` documents a budget of zero as replaying from the start every time, which is
`log₂(n)` slower and produces **identical** answers. A volunteer too small to hold checkpoints
argues slowly. One that died holding them does not argue at all, and is convicted by abandonment.

**One machine, one name, one vote.** Every unit thread reports under the same worker name, and
`coordinator/tests/grid.rs` now has a test that says why.

**Arguing stays on one thread.** Not for simplicity. The referee asks one party one question at a
time, so there is nothing to gain by spreading it out, and the memory a dispute holds is the
unbudgeted cost above.

Free memory is read from `/proc/meminfo` on Linux and assumed conservatively everywhere else. The
alternative is a C call — `GlobalMemoryStatusEx`, `host_statistics64` — and this workspace denies
`unsafe_code` at the root for determinism reasons that have nothing to do with convenience.
`--memory MiB` states it for free, and the startup header says which of the three it used, because
a volunteer that prints "2.0 GiB" when it in fact read nothing is how an operator learns to trust
a guess.

## Consequences

Measured on the development machine — an i5-13500H: **4 performance cores + 8 efficiency cores,
12 physical, 16 hardware threads** — running 400 units of `workloads/examples/busy-loop.wat`
against a local coordinator:

| jobs | wall clock | units/s | speedup | efficiency |
|---:|---:|---:|---:|---:|
| 1 | 56.0 s | 7.14 | 1.00× | 100% |
| 2 | 26.7 s | 14.96 | 2.09× | 105% |
| 4 | 15.1 s | 26.52 | 3.71× | 93% |
| 8 | 10.3 s | 38.81 | 5.43× | 68% |
| 12 | 8.3 s | 48.39 | 6.77× | 56% |
| 15 (default) | 7.7 s | 51.68 | **7.24×** | 48% |

**It bends at four, which is the number of performance cores.** Up to there, scaling is near
linear — 93% efficiency at 4 jobs, and 2 jobs is slightly superlinear because the operating system
is more likely to put at least one thread on a fast core. Past four, each thread added is worth
about a third of the first ones: from 4 jobs to 15, eleven more threads buy 3.5× more work.

**7.2×, not 15×, and the shortfall is the machine rather than the scheduler.** The volunteer
prints each unit's own execution time, which settles it: solo, a unit takes 136 ms; with fifteen
running, 272 ms. Every thread is doing **49%** of its solo work, so the machine's *aggregate*
ceiling under this load is 15 × 0.49 ≈ 7.4×, and Cairn delivers **96%** of it. There is very
little left in the scheduler to win.

That 49% is what a hybrid laptop CPU is: threads land on efficiency cores, which are roughly half
a performance core on scalar arithmetic, and all-core clocks are far below single-core turbo. The
useful form of this finding is general — **donated throughput tracks physical silicon and thermal
headroom, not the logical processor count** — and on a laptop the gap between those two numbers is
close to 2×. A grid that sizes its expectations by counting reported cores will over-promise by
about that much.

Three full repetitions gave 7.05×, 7.24× and 7.38×, so the figure is *about seven* and the third
digit is thermal noise on a laptop. The startup ramp was shortened from 25 ms per thread to 10 ms
during this work; the effect was smaller than the run-to-run spread, which is worth knowing before
anyone tunes it further.

What this costs:

- **The coordinator is now closer to being the bottleneck.** It serves requests one at a time.
  At 15 jobs it handles about 100 requests/second and is nowhere near saturated, but a handful of
  multi-core volunteers would change that, and the fix — a thread pool around `Server::incoming` —
  is deliberately **not** in this change, because it has not yet been measured to be needed.
- **A volunteer is heavier to reason about.** It has threads, a shared cache, and a permit pool.
  The abandonment bug in ADR-0011 came from getting a single-threaded state machine subtly wrong;
  this is a larger surface for that kind of mistake, which is why the exit condition is stated in
  one place and the arguing thread is the last to leave.
- **A `--jobs` flag exists after all**, and could be read as the thing this ADR rejected. It can
  only lower the number, which is the whole distinction: the dangerous direction is not offered.

## Alternatives considered

**A plain `--jobs` flag, defaulting to the core count.** Rejected above. The short version is that
the failure it invites is silent, lands on somebody else's machine, and looks like Cairn being
badly behaved rather than like a mis-set flag.

**Sizing the pool once, from the first workload seen.** Simpler, and wrong as soon as a grid
serves two workloads: either a later, larger workload over-commits the machine, or the count has
to shrink at runtime, and threads already executing overshoot anyway. The byte allowance makes the
question not arise.

**Budgeting against the network's worst case** — every unit assumed to declare the full 256 MiB.
Safe and useless: on an assumed 2 GiB budget it permits four units regardless of what the grid is
actually running, which on most machines is a worse answer than the one it replaces.

**One process per core, each with its own name.** It would work, and it destroys the one-vote
guarantee for a machine that is one failure domain and one operator. Two processes on a 128-core
donor is a deliberate choice by somebody who understands the trade; sixteen processes as the
*default* is that choice made accidentally.

**A dependency for reading free memory** (`sysinfo` and similar). It would give one number on
every platform, at the cost of the rule that a dependency must do something the standard library
cannot. `--memory` does the same job, and being told the number was assumed is more useful than
being given a number of unknown provenance.
