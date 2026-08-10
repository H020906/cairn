# Where to start

**Every issue on this page is closed.** That is not a claim that the project is finished — it
is much less than half built — but the specified, sized, self-contained work is done, and
pretending otherwise by leaving stale tickets open would be worse than saying so.

**What this page is for now** is the record of what each one actually taught, which is the part
that does not survive in a commit log. Four of the nine turned out differently from how they
were written, one of them exactly backwards:

| | |
|---|---|
| **[6b](#6b--make-metering-cheap--done-and-it-does-not-do-what-this-issue-wanted--closed)** | asked for a change that would make disputes cheaper. Measured, it makes them **dearer** — and buys something else nobody had asked for. |
| **[8](#8--dont-canonicalize-nans-that-cannot-happen--done-differently--closed)** | asked for a dataflow analysis. The problem dissolved instead: canonicalize at the four *escapes*, and the analysis is unnecessary. |
| **[7](#7--property-test-the-bisection-game--done-and-better-than-proposed--closed)** | asked for `proptest`. Exhaustive enumeration is strictly stronger and needed no dependency — and it showed the documented `⌈log₂ n⌉` is a band, not a number. |
| **[1](#1--fuzz-the-admission-gate--done-and-it-found-nothing--closed)** | found **nothing** across 12 million inputs, which is reported as the negative result it is. |
| **[5](#5--check-that-runtime-still-compiles-for-wasm32--already-done-and-this-entry-was-stale--closed)** | had already been done before it was written down. |

**If you want to work on Cairn, the work is now the unbuilt half**, and it is large rather than
specified: the coordinator, the schema, the verification policy, the dashboard, a real
scientific workload. [MAINTAINER.md](MAINTAINER.md) §3 says what exists and what does not, §5
the invariants any of it must sit inside. Open an issue saying which you want and it is yours.

Two smaller things are genuinely still open and are noted where they belong: **coverage-guided
fuzzing** with `cargo fuzz` (issue 1), and **comparing generated modules under the honest-path
config** as well as full instrumentation (issue 6).

**Sizes below:** **S** = an afternoon · **M** = a few days · **L** = a week or more.

---

### 1 · ~~Fuzz the admission gate~~ — **done, and it found nothing** · `closed`

> `runtime/tests/admission.rs`. Four seeded generators — random bytes, noise behind a valid
> header, chained mutations of valid modules, and splices of two — 80,000 inputs per run in CI
> and as many as you like locally via `CAIRN_FUZZ_ITERATIONS`. Every input is timed, because a
> hang inside a validator is indistinguishable from a slow CI runner until the job is killed
> with no information about which input did it.
>
> **12 million inputs found no panic and no hang.** That is a negative result and it is
> reported as one: the other two generators in this repository each caught a real defect on
> their first run, and this one did not. 96,177 of those inputs were *admitted*, so the second
> property — **an admitted module must be instrumentable**, or a coordinator has accepted a
> unit it can never dispatch — was exercised rather than assumed.
>
> The suite reports how many inputs got past the magic number and fails if a generator has gone
> vacuous. Without that, a corpus that stopped being valid would leave every generator passing
> while testing nothing but the rejection path.
>
> One suspicion checked and dismissed on the way: `validate.rs` states no limit on locals while
> `image.rs` refuses above 50,000, which looks like a module that could be admitted and then
> never arbitrated. `wasmparser`'s spec validator enforces the same ceiling, so the gate covers
> it — and `MAX_LOCALS_PER_FUNCTION` now says so, and says what breaks if it is raised alone.
>
> **Still open: coverage-guided fuzzing.** `cargo fuzz` needs nightly and a separate crate;
> the note at the bottom of `admission.rs` says exactly how to set it up and to seed it from
> the corpus already there.

<details><summary>Original issue</summary>

### Fuzz the admission gate — it must never panic · **S** · `good-first-issue`

`validate.rs` is the boundary between untrusted bytes and everything else. Its contract is
that *any* input produces `Ok` or `Err`, never a panic and never a hang.

**Start:** `cargo fuzz` or `arbitrary` + `proptest` over raw byte strings, plus mutations of
the valid modules already in `runtime/tests/`.
**Done when:** a fuzz target exists, runs in CI with a short time budget, and the README of
the fuzz directory says how to run it longer locally.

</details>

---

### 2 · ~~An end-to-end worked example~~ — **done** · `closed`

> `cargo run --example dispute`. Prepares a unit, executes it, introduces a party that diverges
> four-fifths of the way through, drives the bisection **by hand printing every round**, and
> adjudicates. It runs in CI, because a document that has stopped running is worse than no
> document.
>
> Driving the challenge by hand rather than calling `dispute::resolve` is the whole point:
> `resolve` is the same loop with the printing taken out, and the loop is the protocol.
>
> Two things it makes visible that no test does. **The liar is simulated by perturbing the
> roots it reports** — not a shortcut but the exact shape of a party whose execution went
> differently, because the coordinator never sees anything but roots. And the witness for the
> disputed instruction carries **zero memory pages**, because that instruction is arithmetic;
> true of most instructions in most programs, and the reason a witness is usually tiny.

<details><summary>Original issue</summary>

### An end-to-end worked example · **S** · `good-first-issue`

There is no single place where you can watch the whole idea happen. Write
`runtime/examples/dispute.rs`: build a module, execute it honestly, execute it again with one
instruction's result corrupted, run the bisection to convergence, adjudicate, print each
round.

**Why it matters:** this is the artefact that makes the project explicable to someone in ten
minutes. Right now that requires reading 1,148 lines of `dispute.rs`.
**Done when:** `cargo run --example dispute` prints a legible round-by-round trace ending in a
verdict, and README links to it.

</details>

---

### 3 · ~~Write the workload author's guide~~ — **done, both halves** · `closed`

> [`docs/WORKLOADS.md`](WORKLOADS.md). The interface, a complete working unit, the six admitted
> proposals, and — the half the original issue asked for and is easiest to skip — **a reason
> for every refusal.** Threads are not refused because they are hard; they are refused because
> two threads interleave differently on different machines and no trace could describe what the
> other one did. Reference types are refused because a state root hashes the operand stack,
> locals, memory, globals and the frame chain, and a reference is none of those.
>
> The rejection table quotes the validator's real message strings, which means it can go stale
> — it was checked against `Rejection`'s `Display` when written.
>
> Two things in it are not in the issue and are worth keeping. **Where a workload reads its
> input decides what a dispute costs**: reading at the end makes divergence late, which is the
> expensive shape, and the committed example does that deliberately. And **`wasm32-wasi` will
> not work** — it emits imports from `wasi_snapshot_preview1` — which is the first wall a real
> author hits.
>
> **And the compiled example, which is where the page turned out to be wrong.**
> `workloads/examples/rust/` is a real Rust work unit that CI builds and puts through the
> admission gate on every push. It answers `bd3e5cfce4250000` — the same eight bytes as the
> hand-written WAT, from different source in a different language.
>
> The guide's link flags, written from memory, did not link. Rust's `wasm32` target lays a
> **1 MiB shadow stack out first**, so any `--max-memory` below about 1 MiB fails with
> `rust-lld: error: initial memory too small` — an error that never mentions stacks and that
> nobody would guess from the page. `-zstack-size=65536` is the missing flag.
>
> That is the argument for compiling an example rather than describing one: **the section most
> likely to be wrong was wrong, and only a build could say so.**

<details><summary>Original issue</summary>

### Write the workload author's guide · **S** · `good-first-issue`

To write a program Cairn can run, you currently have to read `validate.rs` and infer the
contract. It is small and deserves one page:

- Imports come from module `cairn` only. `input(ptr: i32, len: i32) -> i32` and
  `output(ptr: i32, len: i32)` are the whole interface. `charge` is **reserved** — the
  instrumentation pass injects it, and a submitted module importing it is rejected. So is the
  export name `cairn_fuel`, which is the counter the pass appends under the global metering
  encoding ([ADR-0009](adr/0009-metering-through-a-global-the-engines-disagree.md)); a module
  exporting it is rejected whatever it names.
- Admitted features are exactly `validate::admitted_features()`: mutable globals, sign
  extension, saturating float-to-int, multi-value, bulk memory, floats. Everything else —
  threads, SIMD, reference types — is refused, and each refusal has a determinism reason
  worth stating.
- Memory is capped per unit, declared up front; OOM is deterministic and that is the point.

**Done when:** `docs/WORKLOADS.md` exists, a C or Rust "hello, input/output" example compiles
to a module the validator admits, and the rejection reasons are explained rather than listed.

</details>

---

### 4 · ~~Separate the deterministic benchmark columns from the noisy ones~~ — **done** · `closed`

> Both halves landed. The benchmark measures its own error and prints anything smaller than it
> as *not resolved*; the exact metrics moved into `runtime/tests/exact_costs.rs`, which CI runs
> on every push — instruction counts per instrumentation setting, bisection rounds against
> execution length, and witness page counts, all committed as numbers.
>
> Writing it turned up something the benchmark had been quietly overstating: `cargo bench`
> reports a worst-case witness of **one** page, which is true of its workloads and not true in
> general. None of them use `memory.fill`, which reaches as far in one instruction as its
> length says — 100,000 bytes touches two pages. ADR-0001 already said so in prose after being
> corrected away from an `O(1)` claim; there is now a number holding it in place, and the
> benchmark says the figure is a property of its workloads rather than a bound.

<details><summary>Original issue</summary>

### Separate the deterministic benchmark columns from the noisy ones · **M** · `good-first-issue`

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

</details>

---

### 5 · ~~Check that `runtime/` still compiles for `wasm32`~~ — **already done, and this entry was stale** · `closed`

> CI has run `cargo build -p cairn-runtime --lib --target wasm32-unknown-unknown --release` on
> every push since the scaffold, and `rust-toolchain.toml` installs the target. This issue was
> describing work that already existed.
>
> Which is exactly the case the header of this page warns about — *"if you find one of these is
> already done, that is a documentation bug"* — and it went unnoticed for eight commits, so the
> warning was not idle.
>
> The reason it needs `--lib` is worth keeping: `--workspace` would pull in the reference
> engines, and **wasmtime compiles WebAssembly to native code, so it will not itself compile
> *to* WebAssembly.** Only the library has to.

---

### 6 · ~~Widen the generated corpus to whole modules~~ — **done** · `closed`

> Landed. `wasm-smith`, constrained to `validate::admitted_features()` and shaped by its
> `available_imports` / `exports` templates, generates 200 whole modules per run; 146 of them
> execute, the rest hit a Cairn-only ceiling. **It found a real bug on its first run** — `br 0`
> at function scope names WebAssembly's implicit function label and returns, and the
> interpreter had no such label, so it trapped with an internal `StackUnderflow` on a module
> both reference engines completed.
>
> Two configuration traps recorded where they happened: the `exports` template's memory is
> created *on top of* `max_memories`, so `max_memories: 1` yields two memories and the
> validator refuses every module — ask for zero. And `available_imports` / `exports` silently
> require wasm-smith's `wasmparser` feature; without it they panic at generation time.
>
> Still open: generated modules are compared under **full instrumentation only**. Pairing them
> with the honest-path config would compare two different termination stories, since a
> generated module halts because `ensure_termination` injected a counter into it.

<details><summary>Original issue</summary>

### Widen the generated corpus to whole modules · **M** · `help-wanted`

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

</details>

---

### 6b · ~~Make metering cheap~~ — **done, and it does not do what this issue wanted** · `closed`

> Built, measured, and recorded in
> [ADR-0009](adr/0009-metering-through-a-global-the-engines-disagree.md). Two things in the
> text below turned out to be wrong, and the third is the reason the work was worth doing
> anyway.
>
> **"Three arithmetic instructions on the common path" is not achievable.** WebAssembly has
> `local.tee` and no `global.tee`, so accumulate-and-compare is eight instructions. **And the
> threshold test is not needed at all** — it existed to decide when to enter the host, and the
> host was being entered to enforce a ceiling and schedule snapshots, but whoever executes the
> module can already read the count. Cairn's interpreter intercepts the write; a host engine
> does not need to, because under ADR-0005 it produces no trace and its ceiling is its own
> affair. The shipped encoding is `global.get; i64.const N; i64.add; global.set` — four
> instructions, no branch, no call.
>
> **It does not make a dispute cheaper. It makes one dearer.** Measured: **1.14×–1.25× slower
> in the interpreter**, which is the only engine that runs a metered module, and **2×–6× faster
> on wasmtime**, where nothing runs one. `Config::dispute_path()` therefore keeps the host call.
> The issue asked for the opposite of what the measurement supports, which is what measuring is
> for.
>
> **What it does buy is a capability, not a saving.** An engine Cairn does not control can now
> report how much work it did — run the module, read the exported `cairn_fuel` global. Under the
> host-call encoding that was unavailable at any price a volunteer would accept. Nothing consumes
> it yet, so `Config::honest_path()` still meters nothing; the day the coordinator wants to
> account for contributed work rather than count completed units, this is how.

<details><summary>Original issue</summary>

### Make metering cheap — for the dispute path, and no longer urgent · **L** · `help-wanted`

**Read [ADR-0008](adr/0008-a-dispute-costs-an-interpreted-re-execution.md) before starting.**
The +505% figure below is real and nobody pays it: nothing runs the fully instrumented module
on a JIT, because a trace commitment needs machine state no host engine exposes. A challenged
party re-executes under Cairn's **interpreter**, where metering costs 18%–41% — and that is
dwarfed by the interpreter being 37×–142× slower than the JIT in the first place. The change
is still correct and still worth making. It is not the highest-value work any more.

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

</details>

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

### 7 · ~~Property-test the bisection game~~ — **done, and better than proposed** · `closed`

> `runtime/tests/bisection.rs`. The issue suggested `proptest`; **exhaustive enumeration is
> strictly stronger and needed no dependency.** `Challenge` is four integers, so every length
> from 1 to 512 and *every* divergence point within each — about 131,000 complete games — runs
> in under a second, with 20,000 sampled games at lengths up to 2^63 for the arithmetic that
> only matters near the top of the range.
>
> **Checked for teeth before being trusted.** Adding `+ 1` to the midpoint makes 6 of the 10
> tests fail, and the message names the violated invariant: *asked about step 2, outside its
> own bracket [step 0, step 2]*.
>
> Two things it turned up:
>
> **The round count is a band, not a number.** Every document here says `⌈log₂ n⌉`, which is
> right as a *worst case over divergence points* but is not the count for every one — a round
> either raises the floor (halving, rounding up) or lowers the ceiling (rounding down), so
> where `d` falls decides whether the last round is needed. The real statement is
> `⌈log₂ n⌉ - 1 ≤ rounds ≤ ⌈log₂ n⌉`, and the test asserts **both** ends plus that the upper
> one is actually attained — a test pinning only the upper bound would pass on a state machine
> that had silently stopped halving. (`exact_costs.rs` already recorded a case in the lower
> half, 190,028 steps in 17 rounds against `⌈log₂⌉ = 18`, which nobody had noticed was
> informative.)
>
> **The first party is asked exactly one more question than the second**, because the agreed
> root is taken from it alone — sound, since the lower bound only ever moves to a step where
> both answered identically. A refactor "fixing" the asymmetry would cost a round-trip and buy
> nothing, so it is pinned with the reasoning attached.

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

### 9 · ~~Build the fast path~~ — **done, both halves** · `closed`

> `worker-native/` runs a unit on wasmtime, produces a commitment on the interpreter, and
> settles a dispute between two claimed executions. `browser/` does the volunteer half in a
> browser tab with **no Rust, no dependencies and no build step** — a Web Worker around the
> engine already in the page, which is all ADR-0005 leaves for a volunteer to do.
>
> The three engines agree exactly on the bundled unit: Chromium, wasmtime and Cairn's
> interpreter all answer `bd3e5cfce4250000`, and the two that were asked for an instruction
> count both say **850,022** — one by reading an exported global, the other by counting host
> calls. **That agreement is the claim, and it is exact.** The page's *timing* is not a
> controlled measurement and `browser/README.md` says so at length — an earlier version of that
> file quoted a figure that was wrong twice over, and the two errors nearly cancelled.
>
> `cairn-worker prepare` was added for it: the coordinator's job, done once at registration,
> writing the canonical binary every volunteer then runs unchanged. A browser volunteer needs
> no toolchain because it is not allowed to instrument its own work unit.
>
> **What is left is not a browser problem.** There is no coordinator, so there is nothing to
> fetch a unit *from*; the page runs units already in front of it. See MAINTAINER.md §3.

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

The coordinator, the database schema, the dashboard, and the science workload are all unbuilt
(see [MAINTAINER.md](MAINTAINER.md) §3). They are large, they are
well-specified in [ARCHITECTURE.md](../ARCHITECTURE.md), and they are ordinary engineering.
If you want one, take it — open an issue and say so, and it is yours. They are absent here
only because "write a distributed job coordinator" is not a first issue.

That order was deliberate and is worth stating once: **the hard, novel, falsifiable part was
built first**, so that if the window closed early what survived would be the thing nobody else
has rather than a job queue anyone could write. The consequence is the shape you are looking at
— a verification kernel that has been measured, doubted and corrected in public, attached to a
system that does not exist yet.
