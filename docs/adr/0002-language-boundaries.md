# ADR-0002 — Language boundaries: Java for coordination, Rust for execution, no third

- **Status:** Accepted, **with the coordinator's language suspended by
  [ADR-0010](0010-the-referee-executes-so-the-coordinator-is-rust.md)**
- **Date:** 2026-08-07

> **The consequence below about "exactly one narrow seam" is wrong, and it is wrong in a way
> that changed what got built.** It was written before the dispute protocol existed. The
> coordinator is the referee, and refereeing means *executing an instruction* — adjudication
> rebuilds a machine from a state witness and steps it once. So the boundary is not a wire
> format; it is the coordinator calling the execution kernel.
>
> The coordinator is therefore Rust for now. Everything else here — Rust for execution,
> TypeScript for the web surface, no Go — is untouched, and the Java decision is *suspended*
> rather than overturned, with the trigger for revisiting it written down. See
> [ADR-0010](0010-the-referee-executes-so-the-coordinator-is-rust.md).

## Context

Cairn spans three very different workloads:

1. **Coordination** — connection fan-in from many thousands of volunteers, work-unit
   lifecycle, leases, reputation bookkeeping, dispute refereeing, persistence.
2. **Execution** — deterministic interpretation of WebAssembly with instruction-level
   snapshotting and single-instruction replay.
3. **Presentation** — the contributor-facing web surface.

The project brief permitted Java as the backend base, with Go and/or Rust introduced where
high concurrency warranted it. That is an invitation, not an obligation, and a polyglot
codebase imposes real permanent costs: more toolchains in CI, more build systems, a higher
barrier for outside contributors, and duplicated domain types across a language boundary.
Each additional language must therefore earn its place by doing something the existing
ones genuinely cannot.

## Decision

**Three languages, drawn on capability lines, not on taste.**

### Java 21 — the coordinator

Spring Boot 3 on JDK 21. This is the natural home for the work: rich persistence tooling,
mature transactional semantics, first-class PostgreSQL and Redis integration, and a large
pool of contributors who can read it.

Crucially, **Java 21's virtual threads dissolve the historical reason to reach for another
language here.** The coordinator's hard problem is holding many thousands of mostly-idle
WebSocket connections — the classic C10K shape that once mandated an event-loop runtime.
With virtual threads, each connection gets a blocking-style thread at negligible cost, and
the code that results is straight-line and debuggable. The performance argument that would
have justified a Go gateway in 2019 no longer holds in 2026.

### Rust — the execution kernel

The kernel must:

- interpret WebAssembly with **bit-exact determinism** across machines,
- snapshot machine state every `2^k` instructions with low overhead,
- **replay a single instruction in isolation** from a committed state, and
- compile to `wasm32-unknown-unknown` to run inside a browser tab.

No JVM language can do the last of these, and none can do the first three without fighting
the runtime for control over memory layout and floating-point behaviour. Rust is not chosen
here for speed; it is chosen because **precise, reproducible control over execution is the
entire product**, and because it is the only mature option that compiles to both a native
binary and a small browser-loadable WASM module from one codebase.

### TypeScript — the web surface

React 19, Tailwind, three.js. Uncontroversial.

### Go — deliberately not used

There is no Go service in Cairn. The role it would have taken — a high-concurrency
connection gateway in front of the coordinator — is covered by Java 21 virtual threads.
Adding it would mean a fourth toolchain, a fourth set of domain types to keep in sync, and
a network hop, in exchange for no capability the system lacks.

This is recorded explicitly because "the brief mentioned Go" is not a technical reason, and
a future maintainer should know the omission was a decision rather than an oversight.

## Consequences

- The domain model crosses exactly **one** language boundary (Java ↔ Rust), at exactly one
  place: the work-unit / result / trace-commitment wire format. That format is defined once,
  in a schema, and both sides generate from it. Keeping this boundary to a single narrow
  seam is a standing constraint on future design.
- CI must build three toolchains (JDK, Cargo + `wasm32` target, Node). Acceptable.
- Contributors can work meaningfully in one language without touching the others — the
  seam is narrow enough that a Java contributor never needs to read Rust, and vice versa.
- If a future workload genuinely requires a capability none of these three provide, this
  ADR should be superseded rather than quietly worked around.

## Revisit if

- Virtual threads prove insufficient under real connection load (this is measurable —
  benchmark before reopening the question, do not speculate).
- A GPU execution backend is added; that may impose its own language constraints.
