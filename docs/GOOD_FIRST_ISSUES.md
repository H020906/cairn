# Where to start

Nine pieces of real work, sized and specified. Nothing here is busywork invented to look
welcoming — each one either closes a gap I know exists or attacks a claim I know is shaky.

Read [MAINTAINER.md](MAINTAINER.md) §5 first. Every task below sits inside those invariants.

**Sizes:** **S** = an afternoon · **M** = a few days · **L** = a week or more.
If you find one of these is already done, that is a documentation bug and fixing it is a
perfectly good first PR.

---

### 1 · Fuzz the admission gate — it must never panic · **S** · `good-first-issue`

`validate.rs` is the boundary between untrusted bytes and everything else. Its contract is
that *any* input produces `Ok` or `Err`, never a panic and never a hang.

**Start:** `cargo fuzz` or `arbitrary` + `proptest` over raw byte strings, plus mutations of
the valid modules already in `runtime/tests/`.
**Done when:** a fuzz target exists, runs in CI with a short time budget, and the README of
the fuzz directory says how to run it longer locally.

---

### 2 · An end-to-end worked example · **S** · `good-first-issue`

There is no single place where you can watch the whole idea happen. Write
`runtime/examples/dispute.rs`: build a module, execute it honestly, execute it again with one
instruction's result corrupted, run the bisection to convergence, adjudicate, print each
round.

**Why it matters:** this is the artefact that makes the project explicable to someone in ten
minutes. Right now that requires reading 1,148 lines of `dispute.rs`.
**Done when:** `cargo run --example dispute` prints a legible round-by-round trace ending in a
verdict, and README links to it.

---

### 3 · Write the workload author's guide · **S** · `good-first-issue`

To write a program Cairn can run, you currently have to read `validate.rs` and infer the
contract. It is small and deserves one page:

- Imports come from module `cairn` only. `input(ptr: i32, len: i32) -> i32` and
  `output(ptr: i32, len: i32)` are the whole interface. `charge` is **reserved** — the
  instrumentation pass injects it, and a submitted module importing it is rejected.
- Admitted features are exactly `validate::admitted_features()`: mutable globals, sign
  extension, saturating float-to-int, multi-value, bulk memory, floats. Everything else —
  threads, SIMD, reference types — is refused, and each refusal has a determinism reason
  worth stating.
- Memory is capped per unit, declared up front; OOM is deterministic and that is the point.

**Done when:** `docs/WORKLOADS.md` exists, a C or Rust "hello, input/output" example compiles
to a module the validator admits, and the rejection reasons are explained rather than listed.

---

### 4 · Separate the deterministic benchmark columns from the noisy ones · **M** · `good-first-issue`

`benches/cost.rs` reports two kinds of number in one table: wall-clock times (noisy to about
±10% — three ratios in the committed run come out below 1.00×, which is impossible) and exact
counts (instructions executed, bisection rounds, witness pages) which are **perfectly
reproducible**.

Split them. The exact ones can be regression-gated in CI; the noisy ones cannot.

**Done when:** `cargo bench` emits the deterministic metrics to a separate committed file, CI
fails if they change unexpectedly, and wall-clock stays advisory.

**Half of this is already done and is worth reading first.** The benchmark now measures its
own error rather than asserting one — it times pairs of configurations that instrument to
byte-identical modules, and prints any figure smaller than that error as *not resolved*. On
one workload the error is 148%. What is missing is the CI gate on the exact counts.

---

### 5 · Check that `runtime/` still compiles for `wasm32` · **S** · `good-first-issue`

The browser worker will need this crate to build for `wasm32-unknown-unknown`. Nothing
currently stops a dependency from quietly breaking that, and the failure would be discovered
at the worst possible time.

**Start:** add `cargo check -p cairn-runtime --target wasm32-unknown-unknown` to
`.github/workflows/ci.yml`.
**Done when:** it is in CI and green — or it is *not* green, and the issue is reopened with
what broke, which is more valuable.

---

### 6 · Widen the generated corpus to whole modules · **M** · `help-wanted`

**Partly done.** `tests/differential.rs` now generates 300 random *float expressions* per run
and checks them across three engines — Cairn's interpreter, `wasmi`, and `wasmtime` — under
both instrumentation settings. That generator was written narrow on purpose: it targets the
newest and least-proven reasoning in the repository, the escape set in `canon.rs`. It earned
its place immediately, catching a deliberately removed `copysign` escape that the hand-written
cases could not.

What it does not cover is everything else: control flow, memory operations, calls, integer
arithmetic, module shapes.

**Start:** `wasm-smith`, constrained to exactly `validate::admitted_features()`. The awkward
part is shape, not features — a Cairn workload must export `cairn_run` and `memory`, import
only from `cairn`, and declare a memory maximum, and `wasm-smith` will not produce that
without configuration or post-processing. Budget for that.
**Done when:** CI runs a bounded number of generated modules per build with a fixed seed, a
longer nightly run exists, and any failing module is minimised and committed as a permanent
regression case.
**Careful:** the generator must not emit features the validator rejects, or you will spend
your time measuring the validator.

---

### 6b · Make metering cheap — for the dispute path, and no longer urgent · **L** · `help-wanted`

> **Read [ADR-0008](adr/0008-a-dispute-costs-an-interpreted-re-execution.md) before starting.**
> The +505% figure below is real and nobody pays it: nothing runs the fully instrumented module
> on a JIT, because a trace commitment needs machine state no host engine exposes. A challenged
> party re-executes under Cairn's **interpreter**, where metering costs 18%–41% — and that is
> dwarfed by the interpreter being 37×–142× slower than the JIT in the first place. The change
> is still correct and still worth making. It is not the highest-value work any more.
>
> **If you want the change that actually reduces dispute cost, it is issue 6c.**

`canon.rs` charges fuel by injecting `i32.const N; call $charge` at every basic block. In the
interpreter that costs 18%–41%. **On wasmtime it costs +484% to +502%**
([ADR-0007](adr/0007-metering-is-a-jit-problem-not-an-interpreter-problem.md)) — a host call is
cheap next to interpreted arithmetic and brutally expensive next to compiled arithmetic.

Replace it with a module-local mutable global plus a threshold test: three arithmetic
instructions on the common path, entering the host only when a snapshot is actually due.

**Why it matters:** this only affects disputed units, since ADR-0005 moved metering off the
honest path — but 6× is enough that a coordinator might hesitate to open a dispute, and a
verification mechanism nobody wants to invoke is not one.
**Careful:** the counter becomes part of machine state, so it must enter `StateCommitment`, and
a submitted module must not be able to touch it — the same reservation rule that protects
`cairn.charge` (see [MAINTAINER.md](MAINTAINER.md) §5).
**Done when:** the JIT column in `docs/benchmarks.md` drops, the differential gate is still
green across all three engines, and ADR-0007 gains a follow-up reporting the result **whether
or not it improved**.

---

### 6c · ~~Checkpoint the replay~~ — **done** · `closed`

> Landed. `Replay` keeps up to 32 full machine states and resumes from the nearest one, and a
> late-diverging dispute over a 1.9M-step execution went from **1.2 s to 84.6 ms (14.4×)**.
> The follow-up section of
> [ADR-0008](adr/0008-a-dispute-costs-an-interpreted-re-execution.md) records what the estimate
> below got wrong: dispute cost is set by *where the parties diverged*, not by execution
> length, so an early divergence gains nothing and a late one gains everything.
>
> Two traps it walked into, both now regression-tested: laying checkpoints down in a
> preparatory sweep makes short disputes *slower*, and deriving the spacing from the first
> question sets it to 1 — a bisection opens by asking about step 0.

<details><summary>Original issue</summary>

### Checkpoint the replay, and halve what a dispute costs · **M** · `help-wanted`

`dispute::Replay` answers each bisection round by re-executing **from the beginning**, so a
full bisection costs a party `O(n log n)`. The code says so where it happens; it was written
that way because it is obviously correct and the protocol does not care.

The protocol still does not care, which is what makes this safe: keep periodic *full-state*
checkpoints — not the roots the trace commits to, the actual machine state — and resume from
the nearest one below the requested step. That brings a bisection to `O(n)`.

**Why it matters:** [ADR-0008](adr/0008-a-dispute-costs-an-interpreted-re-execution.md) prices
a dispute at roughly 200× a normal execution per party, and about half of that is this. It is
the largest single reduction available in dispute cost, and unlike the metering change it
attacks the part that dominates.
**Careful:** a checkpoint is a performance artefact and must never become a protocol artefact.
Nothing the coordinator sees may depend on how often a party checkpoints, or two honest workers
with different memory budgets would answer differently.
**Done when:** replaying a 2-million-step execution answers a full bisection in time
proportional to one execution rather than twenty, the existing dispute tests still pass
unchanged, and a test pins that the answers are identical with and without checkpoints.

</details>

---

### 7 · Property-test the bisection game · **M** · `help-wanted`

`Challenge` is a pure state machine, which makes it unusually easy to test properly. For any
execution length `n` and any divergence point `d < n`, the protocol must converge on exactly
`d`, in `⌈log₂ n⌉` rounds, from either party's perspective, with no reachable state where
both parties are simultaneously stuck.

**Start:** `proptest` over `(n, d)` and over the sequence of party responses, including a
party that goes silent mid-game.
**Done when:** convergence, round count, and the absent-party default are properties rather
than examples.

---

### 8 · ~~Don't canonicalize NaNs that cannot happen~~ — **done, differently** · `closed`

> Solved by [ADR-0006](adr/0006-canonicalize-nans-at-escapes-on-the-honest-path.md), and not
> the way this issue proposed. Rather than proving a NaN cannot occur, the pass now
> canonicalizes only at the four operations where a NaN's engine-chosen bits could become
> something other than a NaN. The float kernel's honest-path instruction count went from 2.30×
> bare to 1.00×. The original text is kept below because the *reason* it was the top issue —
> and the reason a mistake here is a consensus bug rather than a slow build — has not changed.
>
> If you want the analysis anyway, it is still the right way to speed up the **dispute** path,
> which does still canonicalize after every NaN-producing operation.

<details><summary>Original issue</summary>

### Don't canonicalize NaNs that cannot happen · **L** · `help-wanted`

After [ADR-0005](adr/0005-the-fast-path-cannot-snapshot.md) moved metering and snapshots off
the honest path, Cairn has exactly one large cost left, and this is it. NaN canonicalization
costs **2.30× in instructions and about +150% in time** on the float benchmark, and unlike
metering it *cannot* be deferred to dispute time: it is what makes two honest workers agree.

But most of it is unnecessary. `f64.add` on two operands that are known not to be NaN and not
to be an infinity-minus-infinity case cannot produce a NaN, so the check after it is dead
work. A dataflow analysis over the function — tracking "cannot be NaN" through constants,
loads of known-clean values, comparisons and arithmetic — would let `canon.rs` skip the
injection entirely at those sites.

**Why it matters:** this is the difference between Cairn beating replication on the workloads
it exists to serve and not. Current figures: ≈1.1× where the honest path has no float
arithmetic, ≈2.6× where it does, against replication's 2.0×.
**Careful:** being wrong here is not a performance bug, it is a consensus bug — a skipped
canonicalization that *was* needed makes two honest workers disagree, and the protocol then
convicts one of them. Be conservative: when the analysis is unsure, canonicalize. The
differential gate must stay green, and a new test should pin at least one case where the
analysis *declines* to skip.
**Done when:** `cargo bench` shows the float kernel's instruction ratio below 2.30×, the
differential gate is green, and a short ADR records which operations the analysis can clear
and why that is sound.

</details>

---

### 9 · ~~Build the fast path~~ — **done natively; the browser half remains** · `help-wanted`

> `worker-native/` exists. `cairn-worker run` executes a unit on wasmtime under
> `Config::honest_path()`; `cairn-worker trace` produces a commitment on the interpreter;
> `cairn-worker dispute` bisects two claimed executions and adjudicates. A smoke test asserts
> the two paths agree on a real workload through the actual binary, which is the ADR-0005
> assumption checked end to end rather than inside the differential harness.
>
> **What is left is the browser**, and it is a different job: a Web Worker, JS glue around the
> engine already in the page, a CPU budget, and backing off on battery and metered connections.
> No Rust engine work — the point of ADR-0005 is that the page's own engine is enough.

<details><summary>Original issue</summary>

### Build the fast path · **L** · `help-wanted`

Everything in this repository describes two execution paths: a fast one (the host's own WASM
engine) and a slow one (our interpreter, used only for arbitration). **Only the slow one
exists.** Every measurement here is on the interpreter, and the premise — execute fast,
arbitrate slow — is currently unexercised.

Note that [ADR-0005](adr/0005-the-fast-path-cannot-snapshot.md) made this job *smaller* than
it used to be. The fast path does not produce a trace — it cannot, and that is the finding
that ADR is about. It runs the **determinism-only** module and returns a result. Trace
production is a separate, dispute-time path that runs the fully instrumented module.

**The engine half is done.** `wasmtime` now runs every corpus case and all 300 generated ones,
under both instrumentation settings, and agrees with the interpreter — so "determinism baked
into the binary survives a compiler nobody controls" is tested rather than assumed, and the
honest path's cost on a compiler is measured rather than guessed.

What is missing is the **worker**: the thing that fetches a unit, runs it, and reports. In the
browser that is a Web Worker plus JS glue around the engine already in the page; natively it is
a binary around `wasmtime`. Neither exists, and neither does the coordinator they would talk
to.

**Start:** the native one, since it needs no browser and no server — a binary that takes a
`.wasm` and an input file, runs it under `Config::honest_path()`, and prints the result. Then
the dispute side: re-run the same unit under `Config::dispute_path()` and emit the trace
commitment.
**Done when:** a contributor can run a work unit end to end from the command line, and the
trace it emits on demand is accepted by `dispute::resolve` against one produced by the
interpreter.

</details>

---

## Not on this list, on purpose

The coordinator, the database schema, the browser worker, the dashboard, and the science
workload are all unbuilt (see [MAINTAINER.md](MAINTAINER.md) §3). They are large, they are
well-specified in [ARCHITECTURE.md](../ARCHITECTURE.md), and they are ordinary engineering.
If you want one, take it — open an issue and say so, and it is yours. They are absent here
only because "write a distributed job coordinator" is not a first issue.
