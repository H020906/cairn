# Writing a workload

A Cairn work unit is a WebAssembly module. This page is the whole contract, and it is short —
if you find yourself reading `runtime/src/validate.rs` to answer a question, that is a bug in
this page and worth reporting.

Check any module against the real gate before you believe anything here:

```bash
cargo run -p cairn-worker -- prepare your-workload.wat /tmp/unit.wasm
```

It validates, instruments, and prints the unit's identity — or refuses, with a reason.

---

## The interface

Three functions, one memory, one entry point. There is no filesystem, no clock, no network and
no source of randomness — **not restricted, absent.** A workload that wanted one would be a
workload two honest volunteers could disagree about.

```wat
(module
  (import "cairn" "input"  (func $input  (param i32 i32) (result i32)))
  (import "cairn" "output" (func $output (param i32 i32)))

  (memory (export "memory") 1 16)          ;; initial pages, and a MAXIMUM — required

  (func (export "cairn_run")
    ;; your work
    ))
```

| | |
|---|---|
| `input(ptr, len) -> i32` | Copies up to `len` bytes of the unit's input to `ptr`. **Returns the input's true length**, whatever you asked for, so you can size a buffer with a zero-length probe. |
| `output(ptr, len)` | Records `len` bytes at `ptr` as the unit's result. The last call wins. |
| `cairn_run` | The entry point. Exported by name; its signature is not checked at registration, so a wrong one fails at execution instead. |
| `memory` | Must be exported under exactly this name, and must declare a maximum. |

Both imports are optional. A unit that needs no input does not have to import `input`, and one
that computes for effect rather than for an answer does not have to import `output` — though
the coordinator has nothing to compare in that case, so it is rarely what you want.

### Two reserved names

**`cairn.charge`** may not be imported and **`cairn_fuel`** may not be exported. Both are
injected by the instrumentation pass: the first is the metering hook, the second is the counter
a volunteer's engine reports its work through
([ADR-0009](adr/0009-metering-through-a-global-the-engines-disagree.md)). A module that could
reach either could forge the count of its own execution, so a module naming either is refused
at the gate — `cairn_fuel` whatever kind of export it names.

---

## A complete, working unit

Sum of squares, weighted by the length of the input. It reads its input only to learn how long
it is, which is enough to make two volunteers given different inputs compute different answers.

```wat
(module
  (import "cairn" "input"  (func $input  (param i32 i32) (result i32)))
  (import "cairn" "output" (func $output (param i32 i32)))
  (memory (export "memory") 1 1)

  (func (export "cairn_run")
    (local $i i32) (local $acc i64) (local $n i32)

    (block $done
      (loop $again
        (br_if $done (i32.ge_u (local.get $i) (i32.const 50000)))
        (local.set $acc
          (i64.add (local.get $acc)
                   (i64.mul (i64.extend_i32_u (local.get $i))
                            (i64.extend_i32_u (local.get $i)))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $again)))

    ;; Asking for zero bytes still reports how many are available, so this learns the
    ;; input's length without needing anywhere to put it.
    (local.set $n (call $input (i32.const 0) (i32.const 0)))
    (local.set $acc (i64.add (local.get $acc) (i64.extend_i32_u (local.get $n))))

    (i64.store (i32.const 0) (local.get $acc))
    (call $output (i32.const 0) (i32.const 8))))
```

It is committed as [`workloads/examples/sum-of-squares.wat`](../workloads/examples/sum-of-squares.wat).
Run it:

```bash
cargo run -p cairn-worker -- run workloads/examples/sum-of-squares.wat workloads/examples/input-a.bin
```

### From C or Rust

Any toolchain that emits a `wasm32` module works, provided the output uses nothing outside the
feature set below. There is a working Rust version of the unit above in
[`workloads/examples/rust/`](../workloads/examples/rust) — **CI builds it and puts it through
the admission gate on every push**, so if a compiler release starts emitting something Cairn
refuses, that is a red build rather than a surprise for you.

```bash
cargo build --release --manifest-path workloads/examples/rust/Cargo.toml
cargo run -p cairn-worker -- run \
  workloads/examples/rust/target/wasm32-unknown-unknown/release/cairn_workload_example.wasm \
  workloads/examples/input-a.bin
```

It answers `bd3e5cfce4250000` — the same eight bytes the hand-written WAT produces, which is
what "the unit is the module, not the source" means in practice.

Three things trip people up, and the third is the one nobody guesses:

- **No WASI.** `wasm32-wasi` emits imports from `wasi_snapshot_preview1`, and the only
  importable module is `cairn`. Target `wasm32-unknown-unknown`, and in C use `-nostdlib`.
- **Declare the memory maximum.** No toolchain emits one by default. In Rust, pass
  `-C link-arg=--max-memory=N`; in C, `-Wl,--max-memory=N`. Without it the module is refused.
- **Shrink the shadow stack, or the link fails for a reason it does not name.** Rust's wasm32
  target defaults to a **1 MiB** shadow stack laid out *first*, so any `--max-memory` below
  about 1 MiB fails with `rust-lld: error: initial memory too small` — a message that never
  mentions stacks. Pass `-C link-arg=-zstack-size=65536` alongside it. A Cairn unit is
  straight-line numerical work, and the interpreter bounds call depth explicitly in any case,
  which is what makes deep recursion fail identically everywhere rather than wherever the
  host's native stack ran out.

The whole flag set is in [`workloads/examples/rust/.cargo/config.toml`](../workloads/examples/rust/.cargo/config.toml),
which is three lines and is the thing to copy.

The instrumentation pass drops custom sections, so the name section goes with them and stack
traces lose their symbols. That is deliberate: two builds of the same workload must produce the
same canonical bytes, because that hash is the unit's identity.

---

## What is admitted

Exactly six proposals, and everything else is refused:

| Admitted | |
|---|---|
| mutable globals | |
| sign extension | `i32.extend8_s` and friends |
| saturating float-to-int | `i32.trunc_sat_f64_u` and friends |
| multi-value | more than one result per block |
| bulk memory | `memory.copy`, `memory.fill`, `memory.init` |
| floating point | see the warning below |

Everything outside that list is refused **for a determinism reason, not a scheduling one.**
Each of these was considered and rejected on the merits:

| Refused | Why |
|---|---|
| **threads, shared memory, atomics** | A unit is single-threaded by definition. Two threads interleave differently on different machines, and there is no trace that could describe "what the other thread did". |
| **SIMD** | Several operations have corner cases the specification leaves to the implementation, which is exactly the class of thing that convicts honest volunteers. |
| **reference types, GC, exceptions, tail calls** | State Cairn's commitment does not cover. A state root hashes the operand stack, locals, memory, globals and the frame chain; a reference is not any of those, so two workers could differ in a way the protocol cannot see. |
| **`memory64`, custom page sizes, multiple memories** | The memory commitment is a page tree of a fixed shape. |
| **imported memory or tables** | State that arrives from outside the module is state the unit's identity does not cover. |
| **a `start` section** | All execution must happen under `cairn_run`, or a unit could compute before anyone was watching. |
| **unbounded memory** | Growth that fails at different points on different machines is a disagreement. |

If you need one of these, the answer is not "ask for an exception" — it is that the
verification mechanism cannot see it, and admitting it would make honest workers convictable.
[ADR-0003](adr/0003-determinism-constraints.md) has the argument in full.

### Floating point is admitted, and it is the sharp edge

`f32` and `f64` arithmetic is allowed and is bit-exact across engines, but only because the
instrumentation pass works for it. WebAssembly leaves the *payload bits* of a computed NaN to
the engine, so two honest volunteers can produce NaNs that differ. Cairn rewrites your module
to force one canonical NaN at the four operations where those bits could become something other
than a NaN — a store, a `global.set` on a float global, a `reinterpret`, and `copysign`
([ADR-0006](adr/0006-canonicalize-nans-at-escapes-on-the-honest-path.md)).

You do not have to do anything about this. It is here because it is the one place where the
platform is doing something on your behalf that you might otherwise be surprised by, and
because if you are writing numerical code you should know that **the guarantee is bit-exact
agreement, not IEEE-754 reproducibility with any particular other system.**

### `exp`, `log`, `sin` — you must bring your own, and there is one to bring

WebAssembly has no transcendental functions, and Cairn will not give you any. The only
importable module is `cairn` and it has three functions, none of them arithmetic. So a workload
that needs `exp` has to compile one into itself.

That is not a limitation anybody chose for tidiness. Twenty thousand inputs, twelve functions,
V8 against the platform libm this repository's tests run on — the figure is how often the two
returned **different bits**:

| function | disagree | | function | disagree |
|---|---|---|---|---|
| `cbrt` | **29.80%** | | `exp` | 7.41% |
| `sinh` | **17.82%** | | `tan` | 3.63% |
| `tanh` | **13.77%** | | `ln` | 3.52% |
| `log10` | **8.98%** | | `cos` | 2.54% |

Twelve functions, twelve disagreements. The gaps are a unit or two in the last place, which is
beneath notice in ordinary numerical work and fatal here: Cairn does not compare answers for
closeness, it bisects to the instruction where two workers diverged and rules against one of
them. A host-imported `cbrt` would put two honest volunteers on opposite sides of a dispute
roughly one call in three.

**[`workloads/rust/cairn-math`](../workloads/rust/cairn-math) is a library you can link
against.** Twenty-six functions — `exp`, `exp2`, `expm1`, `ln`, `log2`, `log10`, `ln_1p`, `pow`,
`sin`, `cos`, `tan`, `asin`, `acos`, `atan`, `atan2`, `sinh`, `cosh`, `tanh`, `sqrt`, `cbrt`,
`hypot`, `fmod`, `floor`, `ceil`, `round`, `trunc` — no dependencies, built from nothing but the
arithmetic WebAssembly specifies exactly. It agrees with the platform libm to one or two units
in the last place across 200,000 samples per function, and `runtime/tests/differential.rs` runs
it through four engines and requires **identical** bytes.

You are not obliged to use it. If you bring your own math, the rule you must not break is that
the answer depends only on the module — no host imports, no reading a clock, nothing that could
differ between two machines. Sloppy math compiled into your module gives everyone the same
sloppy answer, which is your problem as a scientist and not a problem for the grid.

**One trap worth naming: do not use `f64::mul_add`.** A fused multiply-add looks like free
accuracy, and on a target without the instruction it compiles to a call to the platform's `fma`
— which puts somebody else's libm back into your module through a door you did not know was
there. `cairn-math` does not use it anywhere, and a test compiles the library and asserts the
resulting module imports exactly `cairn.input` and `cairn.output` and nothing else.

[ADR-0016](adr/0016-math-belongs-in-the-module-not-the-host.md) has the argument in full,
including the part that was not looked for: for the worst-case argument in the format, the
platform's own `sin` returns `-0.2227` where the answer is `1.0`.

---

## Limits

| | Default | |
|---|---|---|
| module size | 32 MiB | Units go to browsers on domestic connections. |
| declared memory maximum | 4,096 pages (256 MiB) | Bounds the depth of the memory commitment. |
| locals per function | 50,000 | The interpreter expands run-length local declarations, so a tiny module could otherwise ask for billions. WebAssembly's own validator enforces the same ceiling. |
| call depth | bounded | Explicit, so recursion fails identically everywhere — unlike a native stack overflow, which would not. |
| instructions | per unit, declared | Exhaustion is deterministic, and that is the point. |

---

## Two things worth knowing before you write a big one

**Where your workload reads its input decides what a dispute costs.** Two executions of the
same unit are identical until they touch something that differs. Reading the input at the very
end — as the example above does — means a disagreement diverges near the *last* instruction,
which is the expensive shape: the bisection runs its full `log₂ n` rounds and each party
replays most of the execution to answer. Reading it at the start makes any disagreement show up
immediately and cost almost nothing. Neither is wrong; the example does the expensive one on
purpose, so the demonstration is not flattering itself.

**Instrumentation costs you approximately nothing, and it is measured rather than promised.**
On a real compiler the honest path is 0% ([docs/benchmarks.md](benchmarks.md)). The metering
and snapshotting that used to be expensive now run only on units somebody disputes
([ADR-0005](adr/0005-the-fast-path-cannot-snapshot.md)), and the NaN handling above costs an
integer workload nothing at all, since the pass only touches functions that contain float
arithmetic.

---

## When it is refused

Every rejection names itself. The full list is `validate::Rejection`, and the ones people
actually hit are:

| Message | What to do |
|---|---|
| `module does not export \`cairn_run\`` | Export the entry point by that exact name. |
| `module does not export \`memory\`` | Export the memory as `memory`. |
| `memory must declare a maximum, or its size would depend on the host` | Add one — `(memory (export "memory") 1 16)`. |
| `import \`X::Y\` is not permitted; the only importable module is \`cairn\`` | Usually WASI. Retarget to `wasm32-unknown-unknown`. |
| `\`cairn::charge\` is reserved for the instrumentation pass` | You imported the metering hook. Remove it; the pass adds it. |
| `\`cairn_fuel\` is reserved for the instrumentation pass` | You exported the reserved counter name. Rename it. |
| `not a valid Cairn module: …` | The specification-level validator refused it under the admitted feature set — the detail names the proposal. Usually autovectorisation emitting SIMD; turn it off. |
| `start sections are forbidden` | Move the work into `cairn_run`. |
| `memory must be defined by the module, not imported` | Declare the memory rather than importing it. |

To see the whole idea end to end, including what happens when two volunteers disagree:

```bash
cargo run --example dispute
```
