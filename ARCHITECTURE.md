# Cairn — Architecture

> A cairn is a pile of stones. Each traveller passing a hard route adds one. No single
> stone is a landmark; the pile is. That is the shape of volunteer computing.

This document explains **what Cairn is**, **the one hard problem it exists to solve**, and
**how the pieces fit**. It is the entry point for anyone — human or agent — picking this
project up cold.

---

## 1. The problem

There are billions of idle CPUs in the world. Volunteer computing has harvested them
before, and it produced real science: Folding@home briefly exceeded the combined power of
the world's top supercomputers during the 2020 pandemic, and its output appears in
peer-reviewed literature.

But the platforms that made that possible (BOINC, 2002) carry two structural costs that
have never been fixed:

**Cost 1 — the participation barrier.** You must download and install a native client.
That single step eliminates the overwhelming majority of potential contributors.

**Cost 2 — the verification tax.** Volunteer machines are *untrusted*. They may return
fabricated results to farm credit, or corrupted results because the RAM is bad or the CPU
is overclocked — historically the more common failure. BOINC's answer is **redundant
execution**: send every work unit to N machines and require a quorum. That works, and it
means **a third to a half of the entire network's power is spent re-doing arithmetic
someone already did correctly.**

Cairn attacks both. Cost 1 is an engineering problem (compile the worker to WebAssembly,
run it in a browser tab, no install). Cost 2 is the interesting one, and it is the reason
this project is worth building.

---

## 2. The core idea: optimistic execution with dispute-time arbitration

The insight is that **verification and re-execution are not the same thing.** BOINC pays
full re-execution cost on every unit in order to catch the rare liar. Cairn pays almost
nothing on the honest path and makes the liar pay on the rare dishonest one.

### 2.1 The fast path

A work unit is a deterministic WebAssembly program plus its input. A volunteer executes it
**once**, at full speed, on the browser's own native WASM engine. Alongside the result,
the worker returns a **commitment to how it got there**: a Merkle root over snapshots of
machine state taken every `2^k` instructions.

```
result  = f(input)
commit  = merkle_root([ state@0, state@2^k, state@2·2^k, ... , state@end ])
```

Producing that commitment costs a snapshot every few thousand instructions — single-digit
percent overhead, not a second full execution.

### 2.2 Deciding what to trust

Most units are accepted on one execution. Confidence comes from three cheap sources
instead of from redundancy:

1. **Canary units.** Units whose correct answer the coordinator already knows, injected
   into the stream indistinguishably from real work. They continuously measure each
   worker's honesty *and* their hardware's integrity, at a tunable sampling rate.
2. **Reputation.** A per-worker posterior on "returns correct results", updated by canary
   outcomes and dispute outcomes. High-reputation workers earn lower replication.
3. **Selective replication.** Units that are high-value, or assigned to low-reputation
   workers, get a second independent execution.

### 2.3 The dispute game

When two workers return different commitments for the same unit, Cairn does **not** re-run
the job. The two workers play an interactive bisection game, refereed by the coordinator:

```
Round 1:  disagree over instructions [0, N)          → both reveal state@N/2
Round 2:  disagree over [0, N/2) or [N/2, N)         → both reveal midpoint of that half
...
Round log₂(N): disagree over a single instruction i
Final:    coordinator executes instruction i alone, on the instrumented interpreter,
          and learns which worker lied.
```

The coordinator's work is **one instruction**, not N: `O(log N)` messages, and compute that
does not grow with the length of the disputed execution. It can execute that instruction
without replaying anything because the parties supply a **state witness** — the small parts of
the machine whole, and memory as only the pages that instruction touches, each with a Merkle
proof binding it to the state root bisection established. This is the same skeleton as an
optimistic rollup's fraud proof, applied to scientific computation instead of financial state.

### 2.4 Why this is the whole project

Everything else here is competent but ordinary engineering. **This is the part that is
genuinely hard and genuinely new for this domain.**

What it does *not* yet do is beat replication on cost. That was the original argument, and
measurement refuted it: instrumentation overhead was assumed at ≈5% and is 13%–201% depending
on the workload, which leaves Cairn cheaper than replication for some shapes and more
expensive for others — including floating point, the shape it exists to serve. The figures,
the decomposition, and the one optimisation that could change them are in
[ADR-0004](docs/adr/0004-measured-cost-supersedes-the-efficiency-claim.md).

What measured exactly as designed is the arbitration itself: dispute cost does not grow with
execution length (a hundredfold longer execution costs six more rounds), and a witness is one
64 KiB page in the worst case observed. The mechanism works; the price is not yet what it
needs to be.

---

## 3. The determinism requirement

The dispute game is meaningless unless **two honest workers running the same unit produce
byte-identical traces.** Non-determinism does not merely degrade Cairn; it turns honest
workers into apparent liars. This constraint drives most of the runtime design.

WebAssembly is deterministic by construction with a small set of known escapes. Cairn
closes each one:

| Escape | Resolution |
|---|---|
| NaN payload bits are implementation-defined | Canonicalize NaNs after every float op |
| SIMD ops with under-specified corner cases | Feature disabled in the validator |
| Threads / shared memory / atomics | Feature disabled; a unit is single-threaded by definition |
| Host clock, entropy, filesystem, network | Not reachable — the host interface exposes none of them |
| Memory-growth failure depends on host RAM | Fixed memory ceiling per unit, declared in the manifest; OOM is deterministic |
| Divergent instruction counting | Explicit fuel metering, counted identically on both paths |

**The top technical risk in this project** is that the fast path (the browser's native WASM
engine) and the slow path (our instrumented interpreter) disagree on some edge case. If
they do, arbitration convicts honest workers. This is addressed by differential fuzzing of
the two engines against each other as a first-class, always-on part of CI — not as an
afterthought.

---

## 4. Components

```
                        ┌──────────────────────────────┐
   browser tab ────────►│                              │
   (WASM worker)        │      Cairn Coordinator       │
                        │      Java 21 / Spring Boot 3 │
   native worker ──────►│                              │
   (Rust + SQLite)      │  assignment · leases ·       │
                        │  reputation · dispute referee│
   React + three.js ───►│                              │
   (dashboard)          └───────┬──────────────┬───────┘
                                │              │
                         PostgreSQL          Redis
                    (units, results,   (queues, leases,
                     traces, disputes)  heartbeats, pubsub)
```

### `runtime/` — Rust
The deterministic execution kernel. Two engines behind one interface:
- **fast**: hand off to the host WASM engine, take periodic state snapshots
- **slow**: a fully instrumented interpreter capable of executing one instruction in
  isolation from a committed state, used only by the coordinator during arbitration

Also owns the Merkle trace commitment, the fuel meter, and the determinism validator that
rejects a workload binary using a forbidden feature.

### `server/` — Java 21 / Spring Boot 3
The coordinator. Work-unit lifecycle, lease management, result intake, canary injection,
reputation, and the dispute referee. Virtual threads carry the WebSocket fan-in; there is
no separate gateway service and no second backend language, because Java 21 does not need
one.

### `worker-browser/` — Rust → WASM
The zero-install volunteer. Runs inside a Web Worker so the page stays responsive, honours
a CPU budget, and backs off on battery power and metered connections. Opening a web page
is the entire onboarding flow.

### `worker-native/` — Rust
For contributors donating a whole machine. Single binary, SQLite for the local unit cache,
checkpointing, and schedule/core limits (e.g. "only overnight, only 6 cores").

### `web/` — React 19 + TypeScript + Tailwind + three.js + GSAP
The contributor-facing surface. The globe renders **live node topology and real throughput
off the WebSocket** — it is instrumentation, not decoration. A contributor should be able
to see what they are computing and why it matters.

### `workloads/`
Real science, compiled to WASM. The first target is molecular docking for virtual drug
screening: one compound per work unit, embarrassingly parallel, deterministic, and
genuinely useful.

---

## 5. Data stores

- **PostgreSQL** — the system of record: projects, workloads, work units, assignments,
  results, trace commitments, disputes, reputation history.
- **Redis** — the hot coordination path: assignment queues, unit leases with TTL,
  heartbeats, and pub/sub for dashboard fan-out. Nothing here is authoritative; a total
  Redis loss costs in-flight assignments, not data.
- **SQLite** — embedded in the native worker for local cache and resumable state. Also
  backs a single-binary local mode so a contributor can run the full loop with no
  infrastructure at all.

---

## 6. What is deliberately not here

- **No blockchain, no token.** The dispute protocol borrows a mechanism from optimistic
  rollups; it does not need a chain, and a token would replace scientific motivation with
  speculative motivation.
- **No GPU workloads in v1.** WebGPU compute is not deterministic enough across vendors for
  the arbitration scheme to hold. CPU first, honestly.
- **No second backend language.** See §4.
- **No trust in the coordinator being unnecessary.** Cairn reduces trust in *workers*. The
  coordinator is still trusted. Removing that is future work, not a v1 claim.

---

## 7. Status

Under active construction. See `docs/adr/` for the decisions behind the above, and the
project roadmap in `README.md` for what is built and what is not.
