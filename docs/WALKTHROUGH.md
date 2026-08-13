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
assumption the whole design rests on, and it is checked by a differential gate against three
independent engines on every push — wasmi, wasmtime, and **the engine in a browser**, reached
through the volunteer's own `host.js`. That third one was added last and is the one that
matters: every volunteer this project is designed for runs it.

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
{"state":"settled","by":"re-execution","verdict":"the second party was wrong ...","refuted":["liar"],"output":"bd3e5cfce4250000"}
```

**Read `"by"`, because there are two routes and they cost very different things.** `curl` did not
declare that it can argue, so this one was settled by the referee executing the unit itself. That
is a route rather than a gap: answering a challenge means producing a state root, and no browser
engine can, so challenging a volunteer that cannot answer would convict it for silence
([ADR-0011](adr/0011-a-volunteer-that-cannot-argue-is-not-challenged.md)).

**Then read `"refuted"`, which is the finding rather than the sentence.** The referee executed this
unit, so it holds the answer and knows which volunteer did not return it. That name goes to
reputation:

```bash
curl -s "http://127.0.0.1:8080/api/reputation"
```

```json
[{"worker":"honest","accepted":1,"canariesPassed":0,"canariesFailed":0,"refuted":0,"lies":0,
  "silences":0,"checkedEvery":250,"standing":{"kind":"unproven","canariesNeeded":3}},
 {"worker":"liar","accepted":0,"canariesPassed":0,"canariesFailed":0,"refuted":1,"lies":0,
  "silences":0,"checkedEvery":250,"standing":{"kind":"provenWrong","failedCanaries":0,
  "refutedResults":1,"provenLies":0}}]
```

**`checkedEvery` is 250 for both, and that is not the mechanism failing.** A volunteer nobody has
seen is already checked hard; what changed is the *standing*. `honest` is `unproven` and can leave
that state — nine clean canaries take it to 30‰, the rate ADR-0001's cost model is written around.
`liar` is `provenWrong`, and nothing takes it out of that state, so it stays at 250‰ however well
it behaves from here. **`is_proven_wrong` deliberately sits outside the posterior**: a thousand
later right answers do not un-do one answer the coordinator can show was wrong.

**`"lies":0` is not an oversight either.** Re-execution proves the *result* wrong and nothing about
why — this route exists because these volunteers cannot argue, which means browsers, and a browser
whose engine diverges from Cairn's interpreter arrives here in good faith. A proven lie needs a
bisection and weighs twenty times as much. See
[ADR-0017](adr/0017-a-verdict-nobody-can-read-is-not-a-verdict.md), which exists because this route
reported all of the above as an English sentence that reputation could not read.

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
the roots afterwards is what makes it convictable.

## 5c · The other kind of wrong, which is more common

Swap the liar for a broken engine — a wrong answer, replayed honestly:

```bash
cargo run --release -p cairn-worker -- volunteer http://127.0.0.1:8080 --name broken --wrong-answer
```

```json
{"state":"settled","by":"bisection","rounds":4,"messages":4,
 "verdict":"nobody lied — both parties' replays agreed, and the trace they agreed on says
            the second party reported the wrong answer (4 messages, nothing executed)"}
```

**Four messages, and nothing executed by anybody.** This party is not caught by bisection at all:
its replay is deterministic, so it reproduces the truth and agrees with the honest party at every
step. Nobody lied, so there is nobody to convict.

It is caught by the trace they agreed on, because **the answer is part of the committed state**.
One witness of the final state, checked against a root both parties had already committed to, and
then two hash comparisons. Before that was true this case cost the coordinator a full interpreted
re-execution — the most expensive path in the system, reached in the *ordinary* case rather than
the adversarial one. See
[ADR-0012](adr/0012-the-answer-is-part-of-the-committed-state.md).

## 5d · What one donated machine is actually worth

A volunteer uses every core it can spare. How many that is, is worked out rather than asked for —
`--jobs` can only lower it — and the reason is in
[ADR-0013](adr/0013-a-volunteer-computes-its-own-parallelism.md).

`workloads/examples/busy-loop.wat` is `sum-of-squares` with a longer loop, sized so that a unit
takes long enough to measure. Queue a few hundred units of it and run one volunteer twice:

```bash
cargo run --release -p cairn-worker -- volunteer http://127.0.0.1:8080 --jobs 1
```

```bash
cargo run --release -p cairn-worker -- volunteer http://127.0.0.1:8080
```

On the machine this was written on — an i5-13500H, 4 performance cores + 8 efficiency cores,
16 hardware threads, 400 units:

| jobs | wall clock | units/s | speedup | efficiency |
|---:|---:|---:|---:|---:|
| 1 | 56.0 s | 7.14 | 1.00× | 100% |
| 2 | 26.7 s | 14.96 | 2.09× | 105% |
| 4 | 15.1 s | 26.52 | 3.71× | 93% |
| 8 | 10.3 s | 38.81 | 5.43× | 68% |
| 12 | 8.3 s | 48.39 | 6.77× | 56% |
| 15 | 7.7 s | 51.68 | **7.24×** | 48% |

**Seven times, from fifteen threads — and the missing half is the machine, not the scheduler.**
Look at where it bends: four, which is exactly the number of performance cores. Each unit line
prints its own execution time, which settles the rest: alone a unit takes 136 ms, and with fifteen
running it takes 272 ms. Every thread is doing 49% of its solo work, so the machine's aggregate
ceiling is about 7.4× and Cairn is getting 96% of it.

That 49% is what a hybrid laptop CPU is — four performance cores, eight efficiency cores at
roughly half the throughput each, and lower clocks when they are all busy. It generalises:
**donated throughput follows physical silicon and thermal headroom, not the core count the
operating system reports.** Anyone sizing a volunteer grid by adding up reported cores will be
out by close to a factor of two.

## 5e · Kill it and start it again

Add `--journal`, and the coordinator writes down every decision it makes:

```bash
cargo run --release -p cairn-coordinator -- workloads/examples/sum-of-squares.wat   workloads/examples/input-a.bin workloads/examples/input-b.bin --journal cairn.journal
```

Kill it partway through — not a shutdown, a `SIGKILL` — and start the same command again:

```
journal       cairn.journal
recovered     1 workloads, 60 units, 16 results, 16 already decided
workload      from the journal; the command line was not used
units queued  60
```

Volunteers reconnect and carry on. The recovery line above is from a larger run than the two
units in that command — 60 units of `busy-loop.wat`, one four-job volunteer, the coordinator
killed outright partway through. Across both lives: **60 units, 60 executions, none lost and none
repeated.** The journal was 17 kB.

It is a log, not a database, and
[ADR-0014](adr/0014-the-coordinator-keeps-a-log-not-a-database.md) is why: every read in the
coordinator is a linear scan of an in-memory `Vec`, so there is nothing for a query engine to do,
and SQLite would put a bundled C library into the component that decides who is convicted of
cheating.

Two things it deliberately does not restore, and both matter more than the storage:

**A unit that was mid-argument comes back unassigned.** A dispute is a live protocol — a blocking
referee, two mailboxes, two volunteers mid-replay — and it cannot be rebuilt from a file. Resuming
it would mean timing out whichever party did not come back, which **convicts an honest volunteer
for the coordinator's crash.** So the argument is voided, the unit is queued again, and the
restarted coordinator prints whose argument it dropped.

**A lease comes back expired.** That sounds like a detail and it is the reason leases are recorded
at all. A lease is two things: *evidence* that a worker was given a unit, and a *reservation*
against other workers. Restoring only the evidence means the volunteer that was mid-unit when the
process died can still hand in its answer — the first version of this feature refused it with
`NotLeased`, and a test caught it — while the unit stays available to everybody else immediately.

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

## 7 · Something that is actually science

Everything above uses `sum-of-squares`, which is a fixture. This is not:

```bash
cargo build --release --manifest-path workloads/periodogram/Cargo.toml

cargo run --release -p cairn-worker -- run \
  workloads/periodogram/target/wasm32-unknown-unknown/release/cairn_periodogram.wasm \
  workloads/periodogram/band-with-the-signal.bin
```

A **Lomb–Scargle periodogram**: given brightness measurements taken at uneven intervals — which is
what a telescope produces, because of daylight and weather and scheduling — it finds how much
periodic signal is in them at each frequency in a band. A peak is a candidate period: a variable
star, a binary, a transiting planet, a pulsar. One frequency band is one work unit, which is the
shape of search Einstein@Home runs.

The answer is three `f64`s: the frequency with the most power, that power, and the total across the
band. The two committed inputs are the **same observations over two different bands**, which is how
a real search is divided among volunteers — so run the other one too:

| unit | peak | power |
|---|---:|---:|
| `band-with-the-signal.bin` | **0.1370 c/d** | **58.1** |
| `band-without-it.bin` | 0.5331 c/d | 1.8 |

The signal was injected at 0.137 cycles per day and comes back at 0.137. The other band has no
signal in it, so its "peak" is the largest fluctuation in noise — and **a power of 1.8 is what pure
noise is supposed to give here**: this workload normalises by the variance, which makes the power
at each independent frequency exponentially distributed with mean 1 under the null hypothesis. The
non-detection is not padding for the demonstration; it is the calibration that makes 58.1 mean
something.

**Why this workload could not have been written six commits ago.** A periodogram is `sin` and `cos`
and almost nothing else, and WebAssembly has neither. Taking them from the host would have
manufactured disputes at roughly the rate the kernel called `sin` — V8 and the platform libm
disagree on the bits of *every* function measured, and on one input the platform is simply wrong
([ADR-0016](adr/0016-math-belongs-in-the-module-not-the-host.md)). It would have presented as an
unexplained dispute rate on a computation nobody could check. That is the reason the math library
came before the science, and
[ADR-0020](adr/0020-the-first-real-workload-is-a-periodogram-not-docking.md) is where the choice of
kernel is argued — including why it is not the molecular docking this project named for a year.

Its test is the only one here that checks the answer is **right** rather than only that every
engine agrees: a signal is synthesised at a known frequency and the peak has to come back at it,
within the resolution the observing span physically allows. Three engines computing the same wrong
number would satisfy every other test in that file.

---

## Checking the claims instead of believing them

```bash
cargo test --workspace        # 319 tests
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
