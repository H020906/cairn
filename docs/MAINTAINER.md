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
- **No dashboard.** `web/` does not exist.
- **The browser worker exists but has nothing to talk to.** `browser/` runs a unit that is
  already in front of it, and everything a volunteer does once a unit is in hand is there and
  works. Fetching one, leases, heartbeats and reporting are all coordinator-shaped and absent.
- **No real workload.** The molecular-docking target is an intention;
  `workloads/examples/sum-of-squares.wat` is a demonstration fixture, not science.

And one thing that *does* exist and is the fastest way in:

- **`worker-native/` — `cairn-worker`.** Four commands: `run` a unit on wasmtime, `trace` one
  on the interpreter, `dispute` two claimed executions end to end, `prepare` the canonical
  binary a coordinator would hand out. Running the third is the shortest path to understanding
  what this project is.
- **`browser/` — the same volunteer, in a tab.** `node browser/server.js`, no toolchain. It is
  worth opening next to `cairn-worker trace`: three engines, one answer, and the same
  instruction count reached two different ways.
- **`cargo run --example dispute` — the ten-minute version.** Every bisection round printed,
  ending in the one instruction the coordinator executes. Read this before `dispute.rs`, not
  after.
- **[WALKTHROUGH.md](WALKTHROUGH.md) — the twenty-minute version.** Five commands with their
  real output. If you are handing this project to someone, hand them that.

`CONTRIBUTING.md` lists JDK, Node and Docker as setup requirements. Today you need Rust, and
node only if you want the browser worker.

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
| 11 | `runtime/tests/differential.rs` | ~1100 | Cairn's interpreter vs `wasmi` **and `wasmtime`**, same bytes, must agree — plus a seeded float-expression generator |
| 12 | `runtime/benches/cost.rs` | ~900 | What verification costs — the *reporting* instrument, run by hand |
| 13 | `runtime/tests/exact_costs.rs` | ~290 | The same costs, the exact ones only — the *gate*, run by CI |

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

**6. `cairn.charge` and `cairn_fuel` are reserved.** A submitted module importing the first or
exporting the second is rejected. They are the two ways `canon.rs` writes a charge — a host
call, or an addition into a counter global the module exports — and a module that could reach
either could forge the count of its own execution. The rule has a second half that is easy to
lose: the counter global is **appended past the module's own index space**, so a validated
module cannot name it even by accident. Keep it that way; the day something inserts a global
instead of appending one, every `global.get` in every workload shifts by one.

**7. The differential gate is not advisory.** It runs in CI and compares Cairn's interpreter
against **two** independent engines on identical instrumented bytes: `wasmi`, which interprets,
and `wasmtime`, which compiles through Cranelift. The second is there because a compiler can go
wrong in ways an interpreter cannot — folding a float expression, contracting a multiply-add,
reassociating arithmetic — and those are precisely the transformations that break bit-exact
agreement. It contains a deliberately-divergent case so the harness cannot pass vacuously, plus
two seeded generators: 300 float expressions and 200 whole `wasm-smith` modules per run. If
this goes red, engines disagree, which is the single largest technical risk in the project.

**8. The admission gate must decide, whatever it is given.** `validate::validate_submitted` is
the only place in Cairn where bytes chosen by a stranger meet code, and everything downstream
assumes it ran. Its contract is total — any input yields `Ok` or `Err`, never a panic and never
a hang. `tests/admission.rs` holds it with four seeded generators and times every input, so a
hang arrives as a named regression case rather than a killed CI job. **12 million inputs have
found nothing**, which is stated as the negative result it is.

**Both generators have caught real defects, which is the argument for keeping them.** The float
one caught a removed `copysign` escape the hand-written cases could not reach. The module one
found, on its first run, that `br 0` at function scope names WebAssembly's implicit function
label and returns — Cairn had no such label and trapped with an internal `StackUnderflow` on a
module both references completed. That is precisely the shape of bug the project cannot
tolerate: Cairn stops, the volunteer's engine continues, they disagree, and arbitration
convicts the honest worker. **If you add a generator, make it fail before you trust it.**

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

**Where that leaves the project: ADR-0001's conclusion holds, at ≈1.11×–1.14× against
replication's 2.00×, on all four workload shapes.** Note carefully that this is not the
original claim being vindicated. ADR-0001 assumed a ≈5% overhead on a path that cannot exist.
The number came back because the honest path now does almost nothing.

**And then the numbers were checked on a real compiler ([ADR-0007](adr/0007-metering-is-a-jit-problem-not-an-interpreter-problem.md)),
which is the part to internalise before trusting any figure here.** Under wasmtime the honest
path costs **0%** — cleaner confirmation than the interpreter could give, because the
interpreter is slow enough to absorb small overheads. **The engine you measure on is part of
the measurement**; most of `docs/benchmarks.md` is the interpreter, and it says so per section.

**Then that ADR was itself corrected a day later, and the correction is the more useful lesson
([ADR-0008](adr/0008-a-dispute-costs-an-interpreted-re-execution.md)).** ADR-0007 measured
metering on a JIT at +505% without first asking *who runs the fully instrumented module*. The
answer is nobody: a trace commitment needs machine state no host engine exposes — the same
argument ADR-0005 makes — so a challenged party cannot produce a trace on their own engine
either. They re-execute under Cairn's interpreter.

So the number that prices a dispute is not instrumentation overhead at all. It is the change of
engine: **37×–142×**, plus the bisection answers, for roughly **200× a normal execution per
party per dispute**. That falls on the two parties, never on the coordinator, whose `O(log n)`
claim is untouched — and it means **the dispute rate has a budget: below about 1 in 4,000
units**. Canary sampling and reputation are therefore load-bearing for *cost*, not just for
confidence.

If you take one habit from this repository, take that one: before optimising a number, check
who pays it.

**Then the optimisation was finally built, and it did the opposite of what three ADRs had
assumed ([ADR-0009](adr/0009-metering-through-a-global-the-engines-disagree.md)).** Metering
through an exported counter global instead of a host call is **3×–6× faster on a compiler and
9%–26% slower in the interpreter** — and the interpreter is the only engine that runs a metered
module, so `Config::dispute_path()` keeps the host call and disputes do not get cheaper. Two
smaller findings on the way there, both of which would have been caught by writing the four
instructions out rather than describing them: WebAssembly has no `global.tee`, so the proposed
"three-instruction threshold test" is eight; and the threshold test is unnecessary, because
whoever runs the module can read the counter without being told.

What the change *does* buy is not a saving but a capability: metering on a compiler falls from
+252%…+563% to +7%…+84%, so **an engine Cairn does not control can now report how much work it
did.** Nothing consumes that yet — `Config::honest_path()` still meters nothing, because a cost
paid on every unit needs a consumer — but it is the answer to the first question a coordinator
will ask, recorded while the measurement is fresh.

**The thing to be careful about here is `canon::escape_site`.** A missing entry is not a
performance regression — it makes two honest workers disagree and the protocol convicts one of
them. It is tested adversarially by `nan_payloads_cannot_escape`, and that test was checked for
teeth: deleting `I64ReinterpretF64` makes it fail, and makes `float_arithmetic_agrees` fail
too. If SIMD is ever admitted, every lane-wise float operation joins this analysis and the
table must be revisited **before** the feature is enabled, not after.

---

## 7. If you have an hour, a day, a week

**An hour.** Clone it and run this:

```bash
cargo run -p cairn-worker -- dispute workloads/examples/sum-of-squares.wat workloads/examples/input-a.bin workloads/examples/input-b.bin
```

A million-instruction disagreement settled by executing one instruction, in about 50µs. Then
read `dispute.rs` from the top — it is the most self-contained interesting thing here, and its
tests read like a specification.

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
