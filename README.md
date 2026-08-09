<div align="center">

# Cairn

**English** · [简体中文](README.zh-CN.md)

**A supercomputer made of spare moments.**

Open a browser tab, donate the CPU you were not using, and help compute something that
matters. No install, no account, no token.

[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Status](https://img.shields.io/badge/status-early%20construction-orange.svg)](#roadmap)

</div>

---

> A cairn is a pile of stones. Each traveller passing a hard route adds one. No single
> stone is a landmark; the pile is.

---

## What this is

There are billions of idle CPUs in the world. Volunteer computing has harvested them
before, and it produced real science — during the 2020 pandemic, Folding@home briefly
exceeded the combined power of the world's top supercomputers.

But the software that made that possible was designed in 2002, and it carries two costs
that were never fixed:

**You have to install something.** That one step loses almost everyone.

**Half the network re-does work that was already done correctly.** Volunteer machines
cannot be trusted — some lie to farm credit, and many more return corrupted results
because the RAM is failing or the CPU is overclocked. The classic defence is to send every
job to two or three machines and compare. It works, and it throws away a third to a half of
all the power people donated.

Cairn attacks both.

The first is engineering: the worker is a Web Worker around the WebAssembly engine the browser
already has. Opening a page is the entire onboarding flow — no install, no dependencies, and
no build step. [`browser/`](browser) does this today.

The second is the reason this project exists. The verification mechanism works, is cheap to
arbitrate, and — after a design correction described below — costs the honest volunteer
essentially nothing on most workloads. Floating point is the exception, and it is the
project's open problem.

## The idea

**Verification and re-execution are not the same thing.**

A volunteer runs the job once and returns the answer. Nothing else — no proof, no trace.
Most jobs are accepted after that **single** execution. Confidence comes from decoy jobs
whose answers we already know, silently mixed into the stream, plus a reputation score
built from how workers handle them.

When two workers *do* return different answers, nobody re-runs the job to see who was right.
Instead, each is asked to show their working: they re-execute under instrumentation and
return a cryptographic commitment to how their execution went. Then they play a bisection
game — binary-searching those commitments down to **the single machine instruction where
their executions first diverged** — and the coordinator executes that one instruction to
find out who lied.

Checking one instruction instead of a billion. `O(log n)` messages, and work that does not
grow with how long the disputed job ran — arbitrating a trillion-instruction unit costs what
arbitrating a thousand-instruction one costs.

The mechanism is borrowed from optimistic rollups and pointed at science instead of
finance. The full design, including why bit-exact determinism is a hard requirement and
what it costs, is in **[ARCHITECTURE.md](ARCHITECTURE.md)**.

## What is measured

`cargo bench` regenerates [docs/benchmarks.md](docs/benchmarks.md). The short version:

| Claim | Status |
|---|---|
| Arbitration cost is independent of execution length | **Confirmed.** 21k steps → 15 rounds; 2.1M steps → 21 rounds |
| A witness is small | **Confirmed, with a caveat now pinned by a test.** One 64 KiB page for ordinary instructions; a `memory.fill` reaches as far as its length says, and 100,000 bytes touches two |
| Metering does not change what a program computes | **Confirmed.** Every differential case, both engines, identical output and trapping |
| Instrumentation overhead is ≈5% | **Refuted.** Nothing like it — see ADR-0004 |
| A volunteer can commit to their own execution | **Refuted.** A stock WASM engine hides four of the seven fields a commitment needs — see ADR-0005 |
| A NaN payload cannot change an answer | **Confirmed** across three engines, including a JIT, on 300 randomly generated float expressions. Checked for teeth: deleting one escape site makes it fail |
| The interpreter agrees with real engines on arbitrary code | **Not yet.** Whole-module generation found a bug on its first run — `br 0` at function scope returns, and Cairn had no function label. Fixed; 146 generated modules now agree. Absence of evidence, so far |
| The honest path costs a volunteer nothing | **Confirmed on a compiler** — 0% under wasmtime on all four shapes, floating point included |
| Cairn beats replication on cost | **Yes, currently.** ≈1.09×–1.18× against replication's ≈2.0×, on all four workload shapes |
| Arbitrating a dispute is cheap **for the coordinator** | **Confirmed.** `O(log n)` messages and one instruction, whatever the execution length |
| Arbitrating a dispute is cheap **for the two parties** | **No.** They must re-execute under Cairn's interpreter to produce a trace at all, and it is 37×–142× slower than the engine they did the work on |
| A party's dispute cost is set by execution length | **Refuted.** It is set by *where the two diverged*. Checkpointed replay makes a late-diverging 1.9M-step dispute **14.4× cheaper** and an early-diverging one no cheaper at all |

That last row took three reversals to get to, and none of them are hidden:

1. Measurement **refuted** the original cost claim — ≈5% assumed, nothing like it in practice
   ([ADR-0004](docs/adr/0004-measured-cost-supersedes-the-efficiency-claim.md)).
2. The execution model underneath it turned out to be **unbuildable** — a stock WASM engine
   will not show you the operand stack — so the trace moved to dispute time
   ([ADR-0005](docs/adr/0005-the-fast-path-cannot-snapshot.md)). That took metering and
   snapshots off the honest path and left only the cost of making floating point bit-exact.
3. That last cost turned out to be avoidable: an engine-chosen NaN only matters where its bits
   can become something other than a NaN, and that is **four operations**
   ([ADR-0006](docs/adr/0006-canonicalize-nans-at-escapes-on-the-honest-path.md)). The float
   kernel's honest-path instruction count went from 2.30× bare to **1.00×**.

**ADR-0001's conclusion is therefore right, and its reasoning is still wrong.** The number came
back because the honest path now does almost nothing, not because the original 5% estimate was
correct. Both facts are in the ADRs.

A fourth reversal followed immediately, and it cuts the other way. Every figure above came from
Cairn's interpreter. Running the same workloads on **wasmtime**, a real optimising compiler,
confirms the honest path is free — and showed fuel metering costing **five to six times** there
against 18%–41% in the interpreter
([ADR-0007](docs/adr/0007-metering-is-a-jit-problem-not-an-interpreter-problem.md)). **The
engine you measure on is part of the measurement.**

Then a fifth, from asking who actually runs that module — and the answer was nobody. A trace
commitment covers the operand stack and every frame's locals, so a challenged party cannot
produce one on their own engine any more than a volunteer could. **They re-execute under
Cairn's interpreter, which is 37×–142× slower than the JIT they did the work on**
([ADR-0008](docs/adr/0008-a-dispute-costs-an-interpreted-re-execution.md)). That is the real
price of a dispute, it falls on the parties rather than the coordinator, and it puts a hard
budget on how often disputes may happen — **below roughly 1 in 4,000 units.**

And a sixth, from finally building the fix the third and fourth had been circling. Metering
through a counter global instead of a host call is **3×–6× faster on a compiler and 9%–26%
slower in the interpreter** — and the interpreter is the only engine that runs a metered module,
so the change makes disputes *dearer*, not cheaper, and the dispute path keeps the host call
([ADR-0009](docs/adr/0009-metering-through-a-global-the-engines-disagree.md)). What it buys is
something nobody asked for and everybody will want: on a compiler, metering falls from **+540%
to +8%**, which means **an engine Cairn does not control can now report how much work it did** —
run the module, read the exported counter. That was unavailable at any price a volunteer would
accept.

**The benchmark measures its own error rather than asserting one**, by timing pairs of
configurations that compile to byte-identical modules. When that error exceeds the effect, the
figure prints as *not resolved* instead of as a result — on one earlier run it reached 148%,
and an earlier version of this benchmark would have reported those numbers as findings.

## Stack

| Layer | Choice | Why |
|---|---|---|
| Coordinator | **Java 21**, Spring Boot 3 | Virtual threads carry the connection fan-in without a second service |
| Execution kernel | **Rust** | Deterministic interpretation and instruction-level replay; the JVM cannot do this |
| Browser worker | **JavaScript**, no build step | Zero-install contribution inside a Web Worker, around the engine already in the page. Rust→WASM was the plan until ADR-0005 made it unnecessary |
| Native worker | **Rust** + SQLite | Single binary, resumable, for donated machines |
| System of record | **PostgreSQL** | Units, results, trace commitments, disputes |
| Hot path | **Redis** | Queues, leases, heartbeats, dashboard fan-out |
| Frontend | **React 19** + TS + Tailwind + three.js | The globe shows live node topology, not an animation |

There is no Go service, and that is deliberate — Java 21's virtual threads already cover
the high-concurrency case, and a language earns its place here by doing something the
others cannot. See [ADR-0002](docs/adr/0002-language-boundaries.md).

## Quick start

Rust, and nothing else installed. Do a work unit:

```bash
cargo run -p cairn-worker -- run workloads/examples/sum-of-squares.wat workloads/examples/input-a.bin
```

Now have two volunteers disagree about that same unit, and find out which one is lying:

```bash
cargo run -p cairn-worker -- dispute workloads/examples/sum-of-squares.wat workloads/examples/input-a.bin workloads/examples/input-b.bin
```

```
disputed length   1050030 instructions
bisection rounds  20
divergence        step 1050016
time to bisect    39.8ms

Adjudicating that one instruction took 52.3µs.
Verdict: the second party was wrong.
```

**That is the entire idea, in one command.** A million-instruction disagreement, settled by
executing *one* instruction — and the referee's 52µs is what does not grow when the execution
does. `cairn-worker trace` shows the other half: the same unit, on the interpreter, producing
the commitment that made the bisection possible.

Or contribute from a browser tab, with no Rust at all:

```bash
node browser/server.js
```

The page runs the same unit on the engine your browser already has — **2.5 ms against the
interpreter's 153 ms** — and reports **850,022 instructions**, exactly the number Cairn's
interpreter reports for **the same bytes**, reached by reading a counter the module exports
rather than by being told. There is no WebAssembly engine in [`browser/`](browser) and there is
not supposed to be; see [its README](browser/README.md) for why that is the design and not a
shortcut.

To check the claims on this page rather than take them:

```bash
cargo test --workspace
```

222 tests, plus twelve more in `node --test browser/policy.test.js`. Among them: an interpreter
checked instruction-by-instruction against **two** independent WASM engines including a JIT, 300
randomly generated float expressions and 200 whole generated modules per run, a bisection game
that converges on a corrupted instruction, and an adjudication that names the liar without
replaying the job. Then `cargo bench` regenerates the numbers above, including the ones that
came out badly.

## Roadmap

Cairn is being built in a deliberately short, fixed window, with a bias toward *narrow and
finished* over *broad and abandoned*.

**What exists is the verification kernel and the two workers.** The stack table above describes
the design; the Java coordinator, the database schema and the dashboard are not in this
repository. That is stated plainly rather than left implied by unticked boxes.

| Milestone | Status |
|---|---|
| Repository, CI, architecture decision records | **Done** — CI runs the real determinism gate, not a placeholder |
| **Deterministic execution kernel + trace commitment** | **Done** — ~11.2k lines of Rust, 222 tests |
| **Interactive bisection arbitration** | **Done** — narrows to one instruction, adjudicates from a state witness, never replays |
| Benchmarks + maintainer handover | **Done** — and the benchmarks refuted three headline claims; see above |
| **Native worker** | **Done** — `cairn-worker`, runs a unit on a JIT and settles a dispute end to end |
| **Browser volunteer** | **Done** — a Web Worker around the page's own engine; no install, no dependencies, no build step |
| Coordinator: domain model, schema, assignment, leases | Not started |
| Verification policy: canaries, reputation, selective replication | Not started |
| Dashboard + live globe | Not started |
| A real scientific workload (molecular docking) | Not started |

That order was deliberate: build the hard, novel, falsifiable part first, so that if the
window closed early, what survived would be the thing nobody else has rather than a job
queue anyone could write.

## Contributing

The project is explicitly built to be picked up by people who did not write it.

- **[docs/MAINTAINER.md](docs/MAINTAINER.md)** — the state of the project, honestly: what
  works, what does not, the seven invariants that must not be broken, and what to do with
  your first hour, day and week.
- **[docs/GOOD_FIRST_ISSUES.md](docs/GOOD_FIRST_ISSUES.md)** — specified pieces of real work,
  sized, each with where to start and how you know you are done. Five are open; the six that
  are closed are kept with what they actually taught, because four of them turned out
  differently from how they were written.
- **[ARCHITECTURE.md](ARCHITECTURE.md)** — how the pieces fit, and why determinism is a hard
  requirement rather than a nice property.
- **[CONTRIBUTING.md](CONTRIBUTING.md)** — setup, tests, and the one rule that is not
  negotiable.

## Licence

[Apache-2.0](LICENSE).
