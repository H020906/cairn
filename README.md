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

Cairn fixes both.

The first is engineering: the worker compiles to WebAssembly and runs in a browser tab.
Opening a page is the entire onboarding flow.

The second is the reason this project exists.

## The idea

**Verification and re-execution are not the same thing.**

When a volunteer finishes a job, they return the answer *and a cryptographic commitment to
how they got there* — a Merkle root over snapshots of machine state taken every few
thousand instructions. That costs a few percent, not a second full run.

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

> Not yet functional — the repository is days old. This is the shape it is being built to.

```bash
docker compose up
```

Coordinator on `:8080`, dashboard on `:5173`, PostgreSQL and Redis provisioned and
migrated. Open the dashboard and click **Contribute** to become a node.

## Roadmap

Cairn is being built in a deliberately short, fixed window, with a bias toward *narrow and
finished* over *broad and abandoned*. Priority order — anything below the line that does
not land is documented rather than half-built.

- [ ] Repository, CI, one-command local stack
- [ ] Architecture decision records
- [ ] Coordinator domain model + PostgreSQL schema
- [ ] Assignment, leases, heartbeats
- [ ] **Deterministic Rust execution kernel + trace commitment**
- [ ] Browser volunteer node
- [ ] Verification: canaries, reputation, selective replication
- [ ] **Interactive bisection arbitration**
- [ ] Native worker
- [ ] Dashboard + live globe
- [ ] A real scientific workload (molecular docking)
- [ ] Benchmarks and maintainer handover

## Contributing

Contributions are welcome, and the project is explicitly built to be picked up by people
who did not write it. Start with [CONTRIBUTING.md](CONTRIBUTING.md) and
[ARCHITECTURE.md](ARCHITECTURE.md).

## Licence

[Apache-2.0](LICENSE).
