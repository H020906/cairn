# A Cairn volunteer in a browser tab

No install, no toolchain, no build step. Open a page and a machine starts contributing.

```bash
node browser/server.js
```

Then <http://127.0.0.1:8787>. The server is twenty lines of node with no dependencies, and it
exists only because a browser will not load an ES module or fetch a `.wasm` over `file://`.

---

## What is not here, and why that is the point

**There is no WebAssembly engine in this directory.** There is a perfectly good one in the
page. A volunteer compiles the canonical module with `WebAssembly.instantiate`, calls
`cairn_run`, and reads the answer out of linear memory — which is the entire honest path.

That sounds like a shortcut and it is the opposite of one. A trace commitment covers the
operand stack, every frame's locals, the frame chain and the program counter, and **no browser
exposes any of them.** A volunteer therefore *cannot* produce a trace, on any engine, ever —
which is [ADR-0005](../docs/adr/0005-the-fast-path-cannot-snapshot.md), and which is why the
fast path is allowed to be nothing but glue. If somebody disputes a result, a different path
re-executes the same unit under Cairn's own interpreter, and that path is Rust and lives in
[`runtime/`](../runtime).

**There is no instrumentation here either.** The bytes this page runs are the canonical binary
a coordinator produced once, at registration:

```bash
cargo run -p cairn-worker -- prepare workloads/examples/sum-of-squares.wat browser/units/sum-of-squares.wasm --count-fuel
```

Their hash is the work unit's identity. A volunteer that could rewrite its own work unit would
be a volunteer whose result means nothing, so it cannot, and it does not need a Rust toolchain
to take part.

## The five files

| file | what it is |
|---|---|
| `host.js` | Cairn's three imported functions, in JavaScript. The whole contract with a volunteer. |
| `worker.js` | The Web Worker. A work unit is a program somebody else wrote; it runs where it cannot touch the page. |
| `policy.js` | **Pure.** When to take work and how much of the machine to use. The only file with decisions in it. |
| `environment.js` | Everything that touches `navigator`, kept apart from everything that decides. |
| `index.html` | The page. |

`policy.js` is separate from `environment.js` for one reason: a volunteer's manners towards the
machine it is running on should be assertions, not comments.

```bash
cd browser && node --test policy.test.js
```

Twelve of them, and they run in CI. What they pin:

- **Data-saving mode outranks everything, including being plugged in.** `Save-Data` is a
  request, not a capability hint.
- **A laptop on battery above 20% works at half width rather than refusing.** An unplugged
  laptop at 80% is a perfectly good volunteer, and a policy that refused it would refuse most
  of the machines people actually use.
- **Missing information is read permissively.** Firefox and Safari do not expose the Battery
  Status API at all; if an absent field meant "refuse", the policy would exclude most of the
  web without ever saying so.
- **A full share never takes the last core.** The page still has to respond to the person who
  opened it, and a browser that stutters is a browser that gets closed.

## The one thing a browser cannot do

**A running WebAssembly call cannot be interrupted from JavaScript.** No timeout, no
cancellation token, no polite request. Once `cairn_run` is entered, that thread belongs to the
workload until it returns. The only lever is `worker.terminate()` from the page, which kills
the thread and loses whatever was in flight — the *Terminate* button does exactly that, and it
is in the page because pretending otherwise would be the dishonest choice.

This is survivable, and [ADR-0009](../docs/adr/0009-metering-through-a-global-the-engines-disagree.md)
says why: **enforcement on the honest path is allowed to be imprecise, because a volunteer who
stops early has produced no answer rather than a wrong one.** The unit is reassigned. Nothing
about verification is weakened. The exact, deterministic instruction ceiling exists on the
dispute path, where a trap is a result two parties have to agree on.

## What it looks like when it agrees

Hand Cairn's interpreter **the same file this page just ran** — not the same workload, the same
bytes:

```bash
cargo run -p cairn-worker -- trace browser/units/sum-of-squares.wasm workloads/examples/input-a.bin
```

```
instrumented  as received — already canonical, not re-instrumented
time          18.7ms
steps         1250038
fuel          850022
result        bd3e5cfce4250000
```

| | engine | result | instructions |
|---|---|---|---|
| this page | Chromium's own | `bd3e5cfce4250000` | **850,022** |
| `cairn-worker trace` | Cairn's interpreter, the same bytes | `bd3e5cfce4250000` | **850,022** |
| `cairn-worker run` | wasmtime, the unmetered module | `bd3e5cfce4250000` | — |

A browser and an interpreter written in two languages, reading a counter the module keeps for
itself, reaching the same number. **That is the claim this page exists to demonstrate**, and it
is exact — no error bar, no run-to-run variation, the same on every machine. It was unobtainable
from a browser at any acceptable price until
[ADR-0009](../docs/adr/0009-metering-through-a-global-the-engines-disagree.md): it cost a host
call per basic block, and a compiler charges +540% for those.

### About the timing, which is the part not to quote

The page reports **0.71 ms per unit** (mean of 30, warmed) against the interpreter's 18.7 ms in
a release build — roughly 26×, inside the 37×–142× band
[ADR-0008](../docs/adr/0008-a-dispute-costs-an-interpreted-re-execution.md) measured under
controlled conditions.

**Neither of those is a controlled measurement, and an earlier version of this file quoted a
figure that was wrong twice over.** It said 2.5 ms against 153 ms — 61× — where the 2.5 ms was a
single cold call sitting at `performance.now()`'s ~0.1 ms resolution floor, and the 153 ms was a
**debug** build of the interpreter. Two errors in opposite directions landed the ratio in
roughly the right neighbourhood, which is the most dangerous way to be wrong.

The page now warms up and times a batch, and labels the number as what it is. If you want a
figure to rely on, take it from `cargo bench`, which interleaves, rotates, and measures its own
error. **What this page demonstrates is agreement, not speed.**

`steps` is 1,250,038 rather than the 1,050,030 the host-call encoding reports, and that gap is
supposed to be there: four injected instructions per charge site instead of two. Fuel is the
number two parties compare; steps is a private coordinate they each keep. See
`runtime/tests/metering.rs`, which pins exactly that.

## What is still missing

There is no coordinator, so there is nothing to fetch a unit *from*. This page runs units that
are already in front of it — bundled, or picked from disk. Everything a volunteer does once a
unit is in hand is here and works; everything about *getting* one is not, and is not a browser
problem. See [ARCHITECTURE.md](../ARCHITECTURE.md).
