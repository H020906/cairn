<div align="center">

# Cairn

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

The first is engineering: the worker compiles to WebAssembly and runs in a browser tab.
Opening a page is the entire onboarding flow.

The second is the reason this project exists — and the part where the honest answer is
currently *partly*. The verification mechanism works and is cheap to arbitrate. Its
instrumentation overhead is not yet low enough to beat replication on every workload. Both
halves of that are measured, and the numbers are below.

## The idea

**Verification and re-execution are not the same thing.**

When a volunteer finishes a job, they return the answer *and a cryptographic commitment to
how they got there* — a Merkle root over snapshots of machine state taken every few
thousand instructions. Instrumenting a program to produce that costs 13%–201% depending on
the workload, which is far more than the design assumed and is discussed honestly below.

Most jobs are then accepted after a **single** execution. Confidence comes from decoy jobs
whose answers we already know, silently mixed into the stream, plus a reputation score
built from how workers handle them.

When two workers *do* disagree, we never re-run the job. Instead they play a bisection game:
they binary-search their commitments down to **the single machine instruction where their
executions first diverged**, and the coordinator re-executes that one instruction to find
out who lied.

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
| A witness is small | **Confirmed.** One 64 KiB page worst case over 20,000 sampled instructions |
| Instrumentation overhead is ≈5% | **Refuted.** 13%–201%, workload-dependent |
| Cairn beats replication on cost | **Not established.** 1.26×–3.14× against replication's ≈2.0× — better for some workloads, worse for others |

The original cost argument is withdrawn and replaced with the measurements in
[ADR-0004](docs/adr/0004-measured-cost-supersedes-the-efficiency-claim.md), which also names
the one optimisation most likely to change the answer.

## Stack

| Layer | Choice | Why |
|---|---|---|
| Coordinator | **Java 21**, Spring Boot 3 | Virtual threads carry the connection fan-in without a second service |
| Execution kernel | **Rust** | Deterministic interpretation and instruction-level replay; the JVM cannot do this |
| Browser worker | **Rust → WASM** | Zero-install contribution, inside a Web Worker |
| Native worker | **Rust** + SQLite | Single binary, resumable, for donated machines |
| System of record | **PostgreSQL** | Units, results, trace commitments, disputes |
| Hot path | **Redis** | Queues, leases, heartbeats, dashboard fan-out |
| Frontend | **React 19** + TS + Tailwind + three.js | The globe shows live node topology, not an animation |

There is no Go service, and that is deliberate — Java 21's virtual threads already cover
the high-concurrency case, and a language earns its place here by doing something the
others cannot. See [ADR-0002](docs/adr/0002-language-boundaries.md).

## Quick start

There is no running system to start yet — see the roadmap. What you *can* do is verify every
claim on this page for yourself, with Rust and nothing else installed:

```bash
cargo test --workspace
```

197 tests. Among them: an interpreter checked instruction-by-instruction against an
independent WASM engine, a bisection game that converges on a corrupted instruction, and an
adjudication that names the liar without replaying the job. Then `cargo bench` regenerates
the numbers in the table above, including the one that came out badly.

## Roadmap

Cairn is being built in a deliberately short, fixed window, with a bias toward *narrow and
finished* over *broad and abandoned*.

**Today, what exists is the Rust verification kernel and nothing else.** The stack table
above describes the design; only its `runtime/` row has been written. There is no Java
source, no database schema, no browser worker and no dashboard in this repository. That is
stated plainly rather than left implied by unticked boxes, because the first honest question
anyone asks is *what can I actually run*, and the answer is `cargo test`.

| Milestone | Status |
|---|---|
| Repository, CI, architecture decision records | **Done** — CI runs the real determinism gate, not a placeholder |
| **Deterministic execution kernel + trace commitment** | **Done** — ~9.4k lines of Rust, 197 tests |
| **Interactive bisection arbitration** | **Done** — narrows to one instruction, adjudicates from a state witness, never replays |
| Benchmarks + maintainer handover | **Done** — and the benchmark refuted a headline claim; see above |
| Coordinator: domain model, schema, assignment, leases | Not started |
| Verification policy: canaries, reputation, selective replication | Not started |
| Browser volunteer node | Not started |
| Native worker | Not started |
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
- **[docs/GOOD_FIRST_ISSUES.md](docs/GOOD_FIRST_ISSUES.md)** — nine specified pieces of real
  work, sized, each with where to start and how you know you are done.
- **[ARCHITECTURE.md](ARCHITECTURE.md)** — how the pieces fit, and why determinism is a hard
  requirement rather than a nice property.
- **[CONTRIBUTING.md](CONTRIBUTING.md)** — setup, tests, and the one rule that is not
  negotiable.

## Licence

[Apache-2.0](LICENSE).
