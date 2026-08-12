# ADR-0016 — Math belongs in the module, not the host

- **Status:** Accepted
- **Date:** 2026-08-12
- **Builds on:** [ADR-0003](0003-determinism-constraints.md), [ADR-0006](0006-canonicalize-nans-at-escapes-on-the-honest-path.md)

## Context

Cairn exists to run scientific work, and scientific work is floating point. Not the four
arithmetic operations WebAssembly specifies — those are the easy part, and ADR-0003 and ADR-0006
already secured them — but `exp`, `log`, `sin`, `pow`. A molecular dynamics kernel, a diffusion
model, a Fourier transform: none of them get past the first page without a transcendental
function.

**WebAssembly has none.** There is no `f64.exp` instruction and there never will be, because
IEEE-754 does not specify what `exp` returns. It requires the four arithmetic operations and
`sqrt` to be correctly rounded, and for everything else it asks only that an implementation be
reasonable. That freedom is the entire subject of this record.

So a workload that needs `exp` has two places to get it: **import it from the host**, or
**compile it into its own module**. The first is the obvious choice. It is one import instead of
a kilobyte of polynomial, it is what every emscripten-compiled module does, and every host
already has a perfectly good `Math.exp` sitting there.

### The measurement

Twenty thousand inputs, twelve functions, V8 against the platform libm this repository's tests
run on. The figure is the fraction of inputs on which the two hosts returned **different bits**:

| function | disagree | | function | disagree |
|---|---|---|---|---|
| `cbrt` | **29.80%** | | `exp` | 7.41% |
| `sinh` | **17.82%** | | `tan` | 3.63% |
| `tanh` | **13.77%** | | `ln` | 3.52% |
| `log10` | **8.98%** | | `cos` | 2.54% |
| `sin` | 2.17% | | `asin` | 1.87% |
| `atan` | 0.32% | | `pow` | 0.01% |

Twelve functions, twelve disagreements, none of them rare. The gaps are one or two units in the
last place, which in ordinary numerical work is beneath notice.

Here it is everything. Cairn settles a disagreement by bisecting to the first instruction at
which two workers diverged and ruling against one of them. It has no notion of *nearly the
same*. A browser volunteer and a native volunteer computing `cbrt` would land on opposite sides
of a dispute on close to one call in three, and arbitration would convict whichever of the two
**honest** volunteers happened to be running the engine that lost.

This is the failure shape ADR-0003 names as the one that matters: non-determinism does not
degrade Cairn, it manufactures false convictions. A host-imported `exp` would have been a
generator of them.

### The second finding, which was not looked for

While testing argument reduction, a stronger fact turned up. For

```
x = 6381956970095103 × 2^797
```

the true remainder `x mod (pi/2)` is `4.687e-19`, so `sin(x)` is `1.0` to every bit a `f64`
holds. This is the documented worst case in the format: `x·(2/pi)` sits within `2^-61` of an
integer, and a reduction must carry more than 110 bits of `2/pi` to recover a single correct bit.

- Exact integer arithmetic over a 3000-bit `pi`, derived from Machin's formula: **1.0**
- V8: **1.0**
- `cairn-math`: **1.0**
- **The platform libm: −0.2227**

Not a rounding difference. A wrong answer, from a shipping production library, with nothing
whatsoever to indicate that anything went wrong. Whatever reduction it uses runs out of
precision and returns a plausible number.

That changes the argument's shape. Host math is not merely *inconsistent between hosts* — it is
not reliably *correct* on any one of them, and a workload that trusted it would produce results
nobody could reproduce and nobody could detect were wrong.

## Decision

**Math never comes from the host. A workload that needs a transcendental function compiles one
into its own module.**

The mechanism was already in place and is now load-bearing rather than incidental: `validate`
admits imports from the module named `cairn` and from nowhere else, and that module has exactly
three functions — `input`, `output`, `charge` — none of them arithmetic. There is no host math
to import even if a workload wanted it.

**`workloads/rust/cairn-math` is what makes the rule livable.** Twenty-six functions, no
dependencies, written from nothing but the operations WebAssembly pins down exactly:

- `+`, `-`, `*`, `/` and `sqrt` — IEEE-754 correctly rounded, mandated
- `floor`, `ceil`, `trunc` — exact by definition
- `abs`, `copysign`, and reinterpretation between `f64` and `u64` — bit manipulation
- integer arithmetic, including the 128-bit multiplies in the argument reduction

The algorithms are fdlibm's, unchanged and attributed. They were not chosen for novelty; they
are the most heavily exercised numeric kernels in existence, and the goal here is
bit-reproducibility rather than a better `exp`.

**Argument reduction is exact, with no fast path.** The trigonometric functions hold `2/pi` as a
table of 64-bit chunks and multiply by the argument's mantissa as integers. There is no rounding
in the reduction, so there is no accuracy cliff at any magnitude — `sin(1e300)` is computed by
the same code, to the same quality, as `sin(1.5)`. A branchy multi-step reduction would be a
seam, and a seam is somewhere for two engines to disagree.

**The table is derived, not transcribed.** A test recomputes `2/pi` from Machin's formula with
big-integer arithmetic and compares every limb, and separately checks that the `pi` it derived
begins `0xC90FDAA22168C234` — a constant published in RFC 3526 and arrived at by an entirely
different route. A single wrong bit in that table would not announce itself: small arguments
barely touch it, and large ones would return numbers of the right magnitude that are simply
wrong.

## Consequences

**The property is tested where every other such claim in this project is tested.**
`the_math_library_computes_the_same_bits_on_every_engine` compiles the library to WebAssembly
and runs a corpus through Cairn's own interpreter, wasmi, wasmtime, and the V8 in a volunteer's
browser, comparing output bytes and fuel. It is also the only case in that file compiled from
Rust by a real toolchain rather than written in the text format, so it doubles as the check that
Cairn admits what a stock toolchain emits.

**Accuracy is measured and published rather than claimed.** Against the platform libm, 200,000
samples per function, the worst disagreement is **one or two units in the last place** for every
one of the twenty-six — including `sin`, `cos` and `tan` at arguments up to `1e308`, at one.
That is a *quality* measurement and it is explicitly not the correctness property: the reference
is not correct either, as the finding above demonstrates. What it is good for is catching a
mistake, because a wrong coefficient produces a hundred units of error, not two.

**Four codegen flags are worth a hundredfold in module size.** Built with `opt-level=3` alone
the probe module is **1,580,252 bytes**. With `lto=fat`, `codegen-units=1`, `panic=abort` and
`strip=symbols` it is **14,894 bytes** — a hundred and six times smaller, and *faster* than the
`opt-level=z` build, which comes out twice that size. The difference is standard-library machinery that only whole-program
optimisation can see is unreachable. A workload is downloaded by every volunteer that runs it
and committed to by the grid, so this is not a packaging detail, and a test asserts a ceiling so
that quietly losing a flag fails rather than costing everyone a hundredfold.

**This does not stop a workload bringing bad math of its own.** Nothing in `validate` can tell a
good `exp` from a bad one; they are both just arithmetic. The rule secured here is narrower and
worth stating precisely: **the answer depends only on the module**, so two honest volunteers
running the same module agree. A workload that compiles in a sloppy `exp` gets consistent
answers that are consistently mediocre, which is a scientific problem for its author and not a
consensus problem for the grid. `cairn-math` exists so that reaching for a good one is the path
of least resistance.

**What C2 inherits.** Widening the admitted instruction set is next, and this record narrows it:
nothing that admits a host-computed numeric result may be added. In particular a fused
multiply-add is not the free accuracy win it looks like — `mul_add` compiles to a call to the
platform's `fma` on any target without the instruction, which puts a libm back into the module
through the side door. `cairn-math` therefore does not use it anywhere, and
`no_host_math_reaches_the_module` compiles the library and asserts that the result imports
exactly `cairn.input` and `cairn.output` and nothing else.
