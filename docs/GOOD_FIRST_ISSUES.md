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

### 6 · Replace the hand-written differential corpus with generated modules · **M** · `help-wanted`

`tests/differential.rs` compares Cairn's interpreter against `wasmi` on the same instrumented
bytes. Today the inputs are hand-written cases, so it tests the divergences I thought of.

**Start:** `wasm-smith`, constrained to exactly `validate::admitted_features()`. Keep every
existing case as a named seed — they were each written for a reason.
**Done when:** CI runs a bounded number of generated modules per build with a fixed seed, a
longer nightly run exists, and any failing module is minimised and committed as a permanent
regression case.
**Careful:** the generator must not emit features the validator rejects, or you will spend
your time measuring the validator.

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

### 8 · Don't canonicalize NaNs that cannot happen — the highest-value change in the repository · **L** · `help-wanted`

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

---

### 9 · Build the fast path · **L** · `help-wanted`

Everything in this repository describes two execution paths: a fast one (the host's own WASM
engine) and a slow one (our interpreter, used only for arbitration). **Only the slow one
exists.** Every measurement here is on the interpreter, and the premise — execute fast,
arbitrate slow — is currently unexercised.

Note that [ADR-0005](adr/0005-the-fast-path-cannot-snapshot.md) made this job *smaller* than
it used to be. The fast path does not produce a trace — it cannot, and that is the finding
that ADR is about. It runs the **determinism-only** module and returns a result. Trace
production is a separate, dispute-time path that runs the fully instrumented module.

**Start:** `wasmtime` as a native stand-in for the browser engine. Run the determinism-only
module across the whole differential corpus and assert the results match the interpreter's.
Then add the dispute-time path: same unit, fully instrumented module, trace commitment
compared against the interpreter's.
**Why it matters most:** determinism is baked into the binary by `canon.rs` rather than
enforced at runtime precisely so that an engine nobody controls can be trusted with it. That
design decision is untested until a second engine runs those bytes and agrees.
**Done when:** two independent engines agree on results for every module in the corpus and on
trace commitments for the instrumented variant, and any disagreement is minimised and filed —
a real disagreement here is the most important bug report this project could receive.

---

## Not on this list, on purpose

The coordinator, the database schema, the browser worker, the dashboard, and the science
workload are all unbuilt (see [MAINTAINER.md](MAINTAINER.md) §3). They are large, they are
well-specified in [ARCHITECTURE.md](../ARCHITECTURE.md), and they are ordinary engineering.
If you want one, take it — open an issue and say so, and it is yours. They are absent here
only because "write a distributed job coordinator" is not a first issue.
