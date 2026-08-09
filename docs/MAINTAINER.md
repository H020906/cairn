# State of the project

*A note for whoever picks this up. Written by the person who put it down.*

Last updated: 2026-08-08, at commit `78cba4e` + the ADR-0005 change.

---

## 1. In sixty seconds

Cairn is meant to be volunteer computing (BOINC, Folding@home) without the two costs that
have followed it since 2002: you must install a client, and the network burns a third to a
half of its power re-running work that was already correct.

The second cost is the interesting one, and it is the only part of Cairn that exists today.
The idea: instead of running every job twice and comparing answers, run it **once**, have
the worker commit to a Merkle root over its own execution, and when two workers disagree,
**binary-search their commitments down to the single instruction where they first
diverged** and re-execute only that. The mechanism is an optimistic rollup's fraud proof,
pointed at science instead of finance.

**That mechanism is built, tested, and measured.** Everything else in this repository is
prose describing what would be built around it.

---

## 2. What actually works

All of it lives in `runtime/`, in Rust, and you can prove every claim below on your own
machine in about two minutes:

```bash
cargo test --workspace
```

197 tests: 184 unit, 11 differential, 2 doc. If they are green, the following is true.

**You can take an untrusted WebAssembly module and make it deterministic.** `validate.rs`
rejects anything with a host-dependent escape (threads, SIMD, reference types, any import
outside the three-function host interface). `canon.rs` then rewrites the module once, at
registration: NaN canonicalization, fuel metering, snapshot hooks. Both execution paths run
the *same instrumented bytes*, which is why the fast path can be a JIT nobody controls.

**You can execute it and commit to how you executed it.** `engine/machine.rs` is an
interpreter whose primitive is `step()` — one instruction — with `run()` as a loop over it.
Every `2^k` steps it hashes the whole machine (`state.rs`) into a snapshot, and the
snapshots Merkle-ize into one root.

**You can settle a disagreement without re-running anything.** `dispute.rs` is the
bisection protocol as a pure state machine, plus adjudication. The two parties narrow to one
instruction in `log₂(n)` rounds; then they hand over a **state witness** — the small parts of
the machine whole, and memory as *only the pages that one instruction touches*, each with a
Merkle proof. The coordinator rebuilds the commitment from the witness, checks it equals the
root bisection already established, executes the single instruction, and knows who lied.

**You have evidence, not just claims.** `cargo bench` regenerates
[benchmarks.md](benchmarks.md). It confirmed the arbitration properties and **refuted** the
cost argument — see §6.

---

## 3. What does not exist

Be clear-eyed about this, because README and ARCHITECTURE describe a whole system:

- **No Java.** No coordinator, no domain model, no REST or WebSocket surface. `server/` is
  not a directory that exists.
- **No database.** No schema, no migrations. `docker-compose.yml` will start PostgreSQL and
  Redis for you and nothing will connect to them.
- **No browser worker, no native worker, no dashboard.** `worker-browser/`,
  `worker-native/`, `web/` — none of them exist.
- **No real workload.** The molecular-docking target is an intention.
- **No fast path.** It has not been written, and every measurement in this repository is on
  the *slow* interpreter. Note that what it has to do got **smaller**: under
  [ADR-0005](adr/0005-the-fast-path-cannot-snapshot.md) it runs a determinism-instrumented
  module and returns a result, rather than producing a trace commitment — which it could not
  have done in any case.

`CONTRIBUTING.md` lists JDK, Node and Docker as setup requirements. Today you need Rust and
nothing else.

---

## 4. Read the code in this order

| # | File | Lines | What it is |
|---|---|---:|---|
| 1 | `runtime/src/lib.rs` | 52 | The map. Start here. |
| 2 | `runtime/src/validate.rs` | 855 | The admission gate — what a module may contain |
| 3 | `runtime/src/canon.rs` | 914 | The instrumentation pass — how determinism gets *baked in* |
| 4 | `runtime/src/fuel.rs` | 410 | The instruction coordinate system |
| 5 | `runtime/src/state.rs` | 586 | Canonical machine state and its hash |
| 6 | `runtime/src/merkle.rs` | 709 | Incremental page tree, and `PartialTree` for proof-only reconstruction |
| 7 | `runtime/src/engine/image.rs` | 989 | Decoder; resolves control-flow targets once, at load |
| 8 | `runtime/src/engine/numeric.rs` | 1293 | Every numeric instruction, as pure stack transformations |
| 9 | `runtime/src/engine/machine.rs` | 1987 | The interpreter, snapshots, witnesses |
| 10 | `runtime/src/dispute.rs` | 1148 | Bisection protocol + adjudication |
| 11 | `runtime/tests/differential.rs` | 529 | Cairn's interpreter vs `wasmi`, same bytes, must agree |
| 12 | `runtime/benches/cost.rs` | 436 | What verification costs |

Then the four ADRs in [docs/adr/](adr/), in numerical order. ADR-0001 is the thesis;
ADR-0004 is the measurement that took a bite out of it.

---

## 5. The invariants — break these and the project inverts

Cairn's failure mode is not "returns a wrong answer". It is **convicting an honest
volunteer**. That is silent, rare, and concentrated on unusual hardware. Every rule below
exists to prevent it.

**1. Two honest workers must produce byte-identical traces.** This is the whole
precondition. Non-determinism does not degrade Cairn — it inverts it. Anything in
`runtime/` that reaches a trace must avoid wall-clock, entropy, I/O, thread scheduling,
address-dependent behaviour, and `HashMap` iteration order.

**2. Fuel is a budget, not an address.** It is charged per *basic block*, so many distinct
machine states share one fuel value. You cannot bisect over fuel. The bisection coordinate
is the **step index**, and `fuel.rs` says so in a comment that was written because an
earlier version of that comment was wrong.

**3. Never fabricate a hash.** `PartialTree::root()` returns `Option<Hash>` and returns
`None` when the proofs do not determine the root. `Judgment::BothWrong` carries
`Option<Hash>` for the same reason. A structure like this must never return a plausible
value it could not derive — a wrong root convicts whichever party's claim failed to match
it. If you find yourself writing `[0u8; 32]` as a placeholder, stop.

**4. Floats are raw bits everywhere in state.** In `state::Value`, a NaN is not equal to
itself and `+0.0` and `-0.0` are *different states* even though Wasm's `f32.eq` says they
are equal. `numeric.rs` uses `total_cmp` for `min`/`max` specifically to avoid float
equality entering the consensus surface. Do not "simplify" it back.

**5. Memory page count is committed one level up.** `merkle.rs` deliberately does *not*
commit the number of pages; `state::hash_memory` binds it. This is load-bearing:
`memory.grow` does not resize the tree, so a page tree root alone cannot distinguish memory
sizes. If you use a `PageTree` root directly in a commitment anywhere else, `memory.grow`
becomes invisible to the protocol.

**6. `cairn.charge` is reserved.** A submitted module that imports it is rejected. It is the
metering hook `canon.rs` injects; a module that could call it could forge its own
instruction count.

**7. The differential gate is not advisory.** It runs in CI, it compares Cairn's interpreter
against `wasmi` on identical instrumented bytes, and it contains a deliberately-divergent
case so the harness cannot pass vacuously. If you touch `canon.rs` or either engine and this
goes red, the engines disagree, which is the single largest technical risk in the project.

---

## 6. What the benchmark did to the thesis

ADR-0001 argued Cairn beats replication: roughly **1.18×** against BOINC's **2.0×**,
assuming instrumentation overhead of about 5%. It labelled that 5% a design target and said
measuring it was a deliverable.

Measured overhead is **13% to 201%**, depending on workload shape. Back into the same
formula: **1.26× to 3.14×**. Cheaper than replication for some workloads, *more expensive*
for others — including floating point, which is the shape the project exists to serve.

[ADR-0004](adr/0004-measured-cost-supersedes-the-efficiency-claim.md) records this and
supersedes ADR-0001's cost section. ADR-0001 is left intact with a correction banner rather
than edited, so the original reasoning and the evidence against it are both readable.

What measured *exactly as designed*: arbitration cost does not grow with execution length
(21k steps → 15 rounds, 2.1M steps → 21 rounds), and a witness is one 64 KiB page in the
worst case observed across 20,000 sampled instructions.

Two caveats are in the ADR and belong in your head too. The wall-clock numbers are noisy to
about ±10%, and the table admits it — three ratios come out below 1.00×, meaning added work
apparently ran faster, which is impossible. And the measurement is on the interpreter, the
slow path; on a browser JIT the metering term would likely get *worse* and the
canonicalization term much *better*. It is neither an upper nor a lower bound. It is the
only number that exists.

### What happened next, and it matters more than the numbers

Two things, recorded in [ADR-0005](adr/0005-the-fast-path-cannot-snapshot.md).

**The fast path could not have worked.** A `StateCommitment` covers seven things; a stock
WebAssembly engine lets its embedder see two of them. The operand stack, a live frame's
locals, the frame chain and the program counter are simply not exposed — not by V8, not by
wasmtime, not by `wasmi`, and not by any engine that compiles WebAssembly to machine code.
So a volunteer could never have committed to their own execution the way ADR-0001 said.

The fix is to stop asking them to. A volunteer returns **the result**; if two results
disagree, both parties re-execute under full instrumentation and *then* the bisection game
runs. Determinism — already a hard requirement — is what makes the re-execution the same
execution. That moved metering and snapshots off the honest path entirely, and honest-path
overhead is now indistinguishable from zero on three of four workloads.

**The benchmark's error bar was fiction.** ADR-0004 claimed ±10%. The harness was timing all
samples of one configuration before starting the next, so CPU frequency drift sat inside every
comparison: two configurations compiling to *byte-identical modules* measured up to 148%
apart. It now interleaves, rotates, rebuilds each image per round, and — the part worth
keeping — **measures its own error from byte-identical pairs and refuses to print any figure
smaller than it.** Three workloads calibrate to ±2%. The integer loop does not calibrate at
all on this machine, and its wall-clock numbers are withdrawn rather than footnoted.

**And then the last cost went too ([ADR-0006](adr/0006-canonicalize-nans-at-escapes-on-the-honest-path.md)).**
NaN canonicalization looked unavoidable — 2.30× instructions on the float benchmark, and
unlike metering it cannot be deferred, because it is what makes two honest workers agree at
all. But an engine-chosen NaN only matters where its bits can become something *other* than a
NaN, and that is four operations: store, `global.set` on a float global, `reinterpret`, and
`copysign`. Everything else — arithmetic, comparisons, branches, `abs`, `neg`, `min`, `max`,
truncation — either yields a NaN or yields the same answer for every payload. So the honest
path canonicalizes at those four sites and nowhere else. The float kernel's instruction count
went from 2.30× bare to **1.00×**.

**Where that leaves the project: ADR-0001's conclusion holds, at 1.12×–1.15× against
replication's 2.00×, on all four workload shapes.** Note carefully that this is not the
original claim being vindicated. ADR-0001 assumed a ≈5% overhead on a path that cannot exist.
The number came back because the honest path now does almost nothing.

**The thing to be careful about here is `canon::escape_site`.** A missing entry is not a
performance regression — it makes two honest workers disagree and the protocol convicts one of
them. It is tested adversarially by `nan_payloads_cannot_escape`, and that test was checked for
teeth: deleting `I64ReinterpretF64` makes it fail, and makes `float_arithmetic_agrees` fail
too. If SIMD is ever admitted, every lane-wise float operation joins this analysis and the
table must be revisited **before** the feature is enabled, not after.

---

## 7. If you have an hour, a day, a week

**An hour.** Clone it, run `cargo test --workspace`, then read `dispute.rs` from the top.
The bisection state machine is the most self-contained interesting thing here, and its tests
read like a specification.

**A day.** Do one item from [GOOD_FIRST_ISSUES.md](GOOD_FIRST_ISSUES.md). The point is less
the change than getting the invariants in §5 into your hands rather than your notes.

**A week.** *Make it real* — the fast path, as ADR-0005 redefines it: a host WASM engine
(`wasmtime` natively, the browser's own eventually) running the honest-path module and
returning a result, plus the dispute-time re-execution that produces the trace. Until this
exists, Cairn has one engine and the premise — execute fast, arbitrate slow — is unexercised,
and every cost figure in this repository comes from the interpreter.

This is now clearly the largest hole. The cost work that used to compete with it for this slot
is done.

---

## 8. Things I would tell you over coffee

**The order of construction was deliberate.** Hard novel part first, ordinary part later, on
the theory that if the window closed early what survived should be the thing nobody else has
rather than a job queue anyone could write. The window closed roughly here. I do not regret
the order, but it does mean you have inherited a jewel with no setting.

**The dispute protocol is a pure state machine on purpose.** `Challenge` has no I/O, no
clock, and no network. Whatever transport eventually carries it — HTTP, WebSocket, a queue —
drives the same struct. Do not let transport concerns leak into it.

**The `Claimant` trait is the seam for the coordinator.** When the Java side arrives, its job
in a dispute is to implement two things: relay rounds between the parties, and call
`adjudicate` at the end. It never replays the unit. If a design ever requires the coordinator
to re-execute a work unit to resolve a dispute, that design has thrown away the entire point.

**Trust model, stated honestly:** Cairn removes trust from *workers*. The coordinator is
still fully trusted — it defines canaries, referees disputes, holds results. Removing that is
a genuinely harder problem and is explicitly not a v1 claim. Do not let the README drift into
implying otherwise.

**Windows notes**, since this was built there: use the GNU host toolchain
(`rustup default stable-x86_64-pc-windows-gnu`) — the MSVC one needs a Visual Studio C++
workload. And write commit messages to a file and use `git commit -F`; PowerShell
here-strings mangle embedded quotes when passed to native commands.

**The most useful thing in the repository might be ADR-0004.** Not because the result is
good — it isn't — but because the project measured its own headline claim, found it wrong,
and said so in the same commit. Whatever you change here, keep that. A project that cannot
publish its own refutations is not doing engineering.
