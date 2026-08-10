# Cairn in twenty minutes

Five commands. Each one answers a question, and by the last you will have watched a
million-instruction disagreement settled by executing a single instruction.

You need Rust. Node only for the last one.

```bash
git clone https://github.com/H020906/cairn && cd cairn
```

---

## 1 · What does a volunteer actually do?

```bash
cargo run -p cairn-worker -- run workloads/examples/sum-of-squares.wat workloads/examples/input-a.bin
```

```
engine        wasmtime (compiled)
instrumented  honest path — determinism only
time          6.0ms
result        8 bytes
              bd3e5cfce4250000
```

**That is the whole honest path.** A compiler runs the unit at full speed and returns eight
bytes. No proof, no trace, no commitment — and no way to produce one, which is the finding
[ADR-0005](adr/0005-the-fast-path-cannot-snapshot.md) is about: a state commitment covers the
operand stack and every frame's locals, and no WebAssembly engine exposes either.

`instrumented  honest path — determinism only` is the module it ran. Not the one you wrote —
the one a coordinator rewrote at registration so that two volunteers cannot disagree about a
NaN. That is the only thing done to your program on this path, and on a real compiler it costs
**0%** ([benchmarks.md](benchmarks.md)).

---

## 2 · What happens when somebody disputes the answer?

```bash
cargo run --example dispute
```

The mechanism, printed round by round. The interesting part is the middle:

```
    round   bracket                 ask at      parties      bracket becomes
    ─────   ─────────────────────   ─────────   ──────────   ─────────────────
        1   [       0,     4221]        2110   agree        [2110, 4221]
        2   [    2110,     4221]        3165   agree        [3165, 4221]
        3   [    3165,     4221]        3693   differ       [3165, 3693]
        …
       12   [    3375,     3377]        3376   differ       [3375, 3376]

    Settled: they agree at 3375 and disagree at 3376.
```

Then:

```
    before          6707c056…5d7c1906
    first claims    d4843ff0…d5dccf5a
    second claims   d5843ff0…d5dccf5a

    witness         1 operand, 1 frame, 0 memory pages
    verdict         the second party was wrong
```

**Nobody re-ran the job.** The coordinator asked twelve questions, then executed one
instruction from a state small enough to fit in this paragraph.

Note the witness: **zero memory pages.** The disputed instruction is arithmetic, which most
instructions are, so the judge needed no memory at all. An ordinary load or store needs one
64 KiB page. That is the whole reason arbitration does not grow with execution length.

---

## 3 · Do it against a real execution

The example simulates the dishonest party. This does not:

```bash
cargo run -p cairn-worker -- dispute workloads/examples/sum-of-squares.wat workloads/examples/input-a.bin workloads/examples/input-b.bin
```

```
disputed length   1050030 instructions
bisection rounds  20
divergence        step 1050016
time to bisect    37.0ms

The two agreed entering that instruction and disagreed leaving it:
  before          a0a71831…144ce867
  first claims    8ce101fa…ab7c7f0a
  second claims   0d291c5f…f8df85f0

Adjudicating that one instruction took 56.6µs.
Verdict: the second party was wrong.
```

Two real executions of the same unit, one of which was given a different input — which stands
in for a liar, or for bad hardware, and **the protocol cannot tell those apart and does not need
to.**

A million instructions, twenty rounds, and **56.6 µs to decide it.** That last number is the
one the project lives on: it does not grow when the execution does.

The divergence is at step 1,050,016 out of 1,050,030 — the very end. That is deliberate. The
example workload reads its input *last*, so the two executions stay identical until then, which
is the most expensive shape a dispute can have. A demonstration that chose the cheap shape would
be flattering itself.

---

## 4 · Where do those commitments come from?

```bash
cargo run -p cairn-worker -- trace workloads/examples/sum-of-squares.wat workloads/examples/input-a.bin
```

```
engine        Cairn's interpreter — the fast engine cannot do this
instrumented  dispute path — metering and snapshots
time          10.5ms
steps         1050030
fuel          850022
snapshots     12
initial root  b5ca3c4d…ea3d9dca
final root    979e855e…aa8b42b3
result        bd3e5cfce4250000
```

Same answer as step 1, **different engine and different instrumentation.** That equality is the
assumption the whole design rests on, and it is checked by a differential gate against two
independent engines on every push.

This is what a challenged party pays: they re-execute under Cairn's interpreter, because their
own engine cannot produce a commitment. Under controlled measurement that costs them **37×–142×**
([ADR-0008](adr/0008-a-dispute-costs-an-interpreted-re-execution.md)) — which is why the dispute
*rate* has a budget, and it is tighter than it looks: below roughly **1 in 4,000 units**.

---

## 5 · Contribute from a browser tab

```bash
node browser/server.js      # then open http://127.0.0.1:8787
```

No install, no dependencies, no build step, and **no WebAssembly engine in `browser/`** — there
is a perfectly good one in the page, and after ADR-0005 running it is all a volunteer has to do.

The page reports the unit's answer *and* how many instructions it took: **850,022**, read out of
a counter the module keeps for itself. Hand the same file to Cairn's interpreter —

```bash
cargo run --release -p cairn-worker -- trace browser/units/sum-of-squares.wasm workloads/examples/input-a.bin
```

— and it says 850,022 too. Two engines, two languages, one exact number, no error bar. Asking a
browser how much work it had done used to cost **+540%**, which nobody would have paid; it now
costs +8%, and that is what [ADR-0009](adr/0009-metering-through-a-global-the-engines-disagree.md)
is about.

The page's *timing* is a different matter and [`browser/README.md`](../browser/README.md) is
blunt about it: `performance.now()` is clamped to about 0.1 ms, so a single timed call of a
small unit is not a measurement. **What this page demonstrates is agreement, not speed.**

---

## Checking the claims instead of believing them

```bash
cargo test --workspace        # 237 tests
cargo bench                   # regenerates docs/benchmarks.md
```

The benchmark is worth running for one reason beyond the numbers: **it measures its own error
rather than asserting one**, by timing configurations that compile to byte-identical modules.
Anything smaller than that error prints as *not resolved* instead of as a result. On one earlier
run the error reached 148%.

That habit is not decoration. Four of this project's headline claims have been refuted by its
own measurements, and every one of them is still in the repository with the evidence against it
— see the reversals in [README.md](../README.md) and the
[ADRs](adr/README.md).

---

## What to read next, in order

| | |
|---|---|
| **[ADR-0001](adr/0001-verification-by-dispute-not-replication.md)** | Why the project exists. 10 minutes. |
| **[ADR-0005](adr/0005-the-fast-path-cannot-snapshot.md)** | The moment the original design turned out to be unbuildable, and what replaced it. |
| **[ADR-0008](adr/0008-a-dispute-costs-an-interpreted-re-execution.md)** | Who actually pays for a dispute. The answer was not the coordinator. |
| **[MAINTAINER.md](MAINTAINER.md)** | The honest state of the project, and the eight invariants that must not be broken. |
| **[WORKLOADS.md](WORKLOADS.md)** | How to write a program Cairn can run. |
| **[ARCHITECTURE.md](../ARCHITECTURE.md)** | How the pieces fit, and what is not built yet. |

If you read only two ADRs, read **0005** and **0008** — the two places where the project asked
*"but who is actually doing this?"* and got an unwelcome answer.
