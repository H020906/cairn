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
fails if they change unexpectedly, and wall-clock stays advisory with its noise caveat
attached.

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

### 8 · Make metering cheap — the highest-value change in the repository · **L** · `help-wanted`

`canon.rs` charges fuel by injecting a call to the host function `cairn.charge` at every
basic block. Measurement says that is the dominant cost: in the integer-loop workload the
*instruction count* rose only 1.27× while *time* rose 3.16×. The problem is not how many
instructions the pass adds — it is that the added one is a call across the host boundary.

Replace it with a module-local global counter plus a threshold test: three arithmetic
instructions on the common path, host call only when a snapshot is actually due.

**Why it matters:** this is the change that could recover ADR-0001's conclusion. Overhead is
currently 13%–201% against an assumed 5%, which is what put Cairn between 1.26× and 3.14×
versus replication's 2.0×.
**Careful:** the counter is now part of the machine state, so it must be in the commitment,
and a submitted module must not be able to touch it — see the `cairn.charge` reservation rule
in [MAINTAINER.md](MAINTAINER.md) §5.
**Done when:** `cargo bench` shows the new numbers, the differential gate is still green, and
ADR-0004 gains a follow-up section reporting the result **whether or not it improved**.

---

### 9 · Build the fast path · **L** · `help-wanted`

Everything in this repository describes two execution paths: a fast one (the host's native
WASM engine, taking periodic snapshots) and a slow one (our interpreter, used only for
arbitration). **Only the slow one exists.** Every measurement here is on the interpreter, and
the premise — execute fast, arbitrate slow — is currently unexercised.

**Start:** `wasmtime` as a native stand-in for the browser engine. Run the *same instrumented
bytes*, take snapshots at the same step boundaries, and assert the roots equal the
interpreter's across the whole differential corpus.
**Why it matters most:** the fast path cannot be reached into — that is the entire reason
determinism is baked into the binary by `canon.rs` rather than enforced by the engine. That
design decision is untested until a second engine runs the same bytes and agrees.
**Done when:** two independent engines produce identical trace commitments for every module
in the corpus, and any disagreement is minimised and filed — a real disagreement here is the
most important bug report this project could receive.

---

## Not on this list, on purpose

The coordinator, the database schema, the browser worker, the dashboard, and the science
workload are all unbuilt (see [MAINTAINER.md](MAINTAINER.md) §3). They are large, they are
well-specified in [ARCHITECTURE.md](../ARCHITECTURE.md), and they are ordinary engineering.
If you want one, take it — open an issue and say so, and it is yours. They are absent here
only because "write a distributed job coordinator" is not a first issue.
