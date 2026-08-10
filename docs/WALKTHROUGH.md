# Cairn in twenty minutes

Six commands. Each one answers a question, and by the last you will have watched a
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

## 5 · The whole system

```bash
cargo run -p cairn-coordinator -- workloads/examples/sum-of-squares.wat \
  workloads/examples/input-a.bin workloads/examples/input-b.bin
```

Open the printed address, press **Start contributing**, then open a *second* tab and do the
same. Two volunteers, and the units go from `open` to `accepted` as they confirm each other:

```
unit   0     0.90 ms       850,022 instructions  → open
unit   1     0.40 ms       850,022 instructions  → open
```

`open` because a replicated unit needs a **different** volunteer. The coordinator will not hand
one machine the same unit twice — a quorum of two means two independent executions, and a unit
replicated back to the same machine would agree with itself however broken that machine was.
That one rule is the load-bearing line in the scheduler.

Now be the liar:

```bash
curl -s "http://127.0.0.1:8080/api/lease?worker=liar"
curl -s -X POST "http://127.0.0.1:8080/api/result?unit=0&worker=liar" -d deadbeefdeadbeef
```

```json
{"state":"settled","by":"re-execution","verdict":"the second party was wrong ...","output":"bd3e5cfce4250000"}
```

**Read `"by"`, because there are two routes and they cost very different things.** `curl` did not
declare that it can argue, so this one was settled by the referee executing the unit itself. That
is a route rather than a gap: answering a challenge means producing a state root, and no browser
engine can, so challenging a volunteer that cannot answer would convict it for silence
([ADR-0011](adr/0011-a-volunteer-that-cannot-argue-is-not-challenged.md)).

## 5b · The same dispute, bisected instead

Two volunteers that *can* argue. Start the coordinator so every unit is replicated:

```bash
cargo run --release -p cairn-coordinator -- workloads/examples/sum-of-squares.wat \
  workloads/examples/input-a.bin --replicate 100
```

Then, in two more terminals:

```bash
cargo run --release -p cairn-worker -- volunteer http://127.0.0.1:8080 --name honest
```

```bash
cargo run --release -p cairn-worker -- volunteer http://127.0.0.1:8080 --name liar --lie-from 500000
```

The liar's terminal shows the bisection converging on it, one question at a time:

```
challenge     root at step 525015 — answered in 65.4µs (accepted)
challenge     root at step 262507 — answered in 35.8µs (accepted)
...
challenge     root at step 499999 — answered in 219.2µs (accepted)
challenge     root at step 500000 — answered in 191.0µs (accepted)
```

and `http://127.0.0.1:8080/api/disputes` ends with:

```json
{"rounds":20,"messages":47,
 "conclusion":"the second party lied about the instruction at step 499999, found in 20 rounds
               of bisection and proved by executing that one instruction"}
```

**1,050,030 instructions. 47 messages. One instruction executed by the coordinator.** The honest
party supplied the disputed state as a proof-carrying witness — 10.4 ms — and the referee stepped
a single instruction to see which of the two claims about it was true. Nobody re-ran the unit.

Note `--lie-from`: **a liar has to lie twice.** The wrong answer starts the dispute; corrupting
the roots afterwards is what makes it convictable. A party that returns a wrong answer and then
replays honestly agrees with everybody — the replay is deterministic — so it is not caught by
bisection at all. It is merely wrong, and the re-execution route names it. Both outcomes are
correct; only one of them is cheap.

## 6 · Just the browser worker, with no coordinator

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
cargo test --workspace        # 271 tests
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
