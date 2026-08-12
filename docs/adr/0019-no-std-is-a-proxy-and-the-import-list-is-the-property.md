# ADR-0019 — A workload SDK, and `no_std` is the wrong guarantee to reach for

- **Status:** Accepted
- **Date:** 2026-08-12

## Context

The roadmap's test for this item was *"somebody who is not me compiles a workload"*. Before it,
what that person had to get right was:

- target `wasm32-unknown-unknown`, not `wasm32-wasi`;
- three link flags, together;
- `crate-type = ["cdylib"]`;
- an `extern` block declaring two host functions with the right signatures;
- `#[no_mangle] pub extern "C" fn cairn_run`;
- a fixed buffer and the pointer casts to hand it to the host, since there is no allocator;
- `panic = "abort"` and, for a `no_std` crate, a panic handler.

Seven things, and **failing any of them produces an error that names something else.** The link
flags are the sharp case, measured one step at a time:

| What an author would try | What they get |
|---|---|
| `--target wasm32-unknown-unknown --crate-type cdylib` | Cairn: `memory must declare a maximum` |
| add `--max-memory=131072` | `rust-lld: maximum memory too small, 1114112 bytes needed` |
| add `--initial-memory=131072` | `rust-lld: initial memory too small, 1048584 bytes needed` |
| add `-zstack-size=65536` | it works |

Those numbers are Rust's **1 MiB default shadow stack**, laid out first by `wasm-ld`. Neither
message contains the word *stack*. An author who reads them literally raises the memory ceiling
until the module links and ships a workload reserving a megabyte it never touches — which every
volunteer then holds while running it, and which `worker-native/src/capacity.rs` charges against
how many units that machine can run at once.

## Decision

**A crate, a template, and a command.**

**`workloads/rust/cairn-workload`** is a zero-dependency SDK: `read_input`, `write_output`, an
`Answer` that accumulates into a buffer you declare, and a `workload!` macro that generates the
entry point. Overflowing a declared buffer **traps** rather than truncating, because a short
answer that looks like an answer is the failure this project exists to avoid, and a trap is not
one — it is deterministic, so every honest volunteer produces it and the network agrees on it.

**`workloads/template`** is a copy-and-go cargo package: the manifest, the release profile, and a
`.cargo/config.toml` carrying the three link flags with the measurement above written in its
comments. `cargo build --release` is the whole of the build command; `--target` is in the config.

**`cairn-worker check <module>`** answers the first question an author has — *will this be
accepted* — without needing an output path, and when the answer is no it prints the fix and not
only the rule. The `UnboundedMemory` hint names all three flags at once, precisely because the
error messages between here and there name none of them.

### And `no_std` is not part of it

The obvious shape for a workload is `no_std`: no operating system underneath, no chance of
reaching for a clock or an allocator. The first draft of the template was `no_std` and the
`workload!` macro planted a `#[panic_handler]`.

It does not compose with `cairn-math`, and the reason is worth recording:

```
error[E0152]: found duplicate lang item `panic_impl`
  = note: the lang item is first defined in crate `std` (which `cairn_math` depends on)
```

`cairn-math` cannot be `no_std`. Five of the functions it is built on —

```
f64::sqrt   f64::floor   f64::ceil   f64::trunc   f64::round_ties_even
```

— are **single WebAssembly instructions** (`f64.sqrt`, `f64.floor`, `f64.ceil`, `f64.trunc`,
`f64.nearest`), and Rust puts all five in `std` rather than `core`, because on an ordinary target
they are libm calls. On `wasm32-unknown-unknown` they compile to the instruction and import
nothing.

**So `no_std` was serving as a proxy for the property Cairn actually needs, and that property is
directly checkable.** What matters is that nothing comes from the host, and a module's import
section says whether it does. The template is a `std` crate with `panic = "abort"`, and
`the_workload_template_compiles_and_is_admissible` requires its import list to be exactly
`cairn.input` and `cairn.output`. If `std` ever dragged in an allocator hook, a clock, or a libm
call, it would appear there as a third import.

`workload!` therefore does not plant a panic handler. `abort_on_panic!` is separate, for a genuine
`no_std` workload that has no such dependency.

## Consequences

**What it buys.** The seven things become one: copy a directory. The acceptance test runs the
documented command against the documented directory and requires a module Cairn takes — so it is
the *instructions* that are under test, not only the code.

**What it costs.** `std` on this target, which measures nothing in the module: the template is
**319 bytes** and imports exactly its two host functions. `panic = "abort"` removes the unwinding
machinery, and dead-code elimination removes the rest.

**What is now checked that was not.** That `cairn-math` imports no host math was already checked
in `cairn-math/tests/wasm.rs`. The same check now covers a workload built the way an author builds
one, through cargo, with the template's profile — which is a different code path and the one
people will actually use.

**What this does not solve.** The template is Rust. A C or C++ author still assembles the three
flags by hand, and `docs/WORKLOADS.md` spells them out with the measurement attached. A C
template would be the obvious next piece, and it needs somebody who will actually use it to say
whether it is worth having.

## Alternatives considered

**Make `cairn-math` `no_std` by implementing the five functions in software.** Rejected, and it is
the alternative worth being clear about. `f64.sqrt` is one of the two things IEEE-754 specifies as
correctly rounded and WebAssembly pins down exactly; replacing it with a hand-written routine
would trade a specified instruction for an unspecified one of ours, in the one library whose
entire purpose is to not do that. `floor`/`ceil`/`trunc` are easy in bit arithmetic and would have
been fine — but they are not the ones that matter, and mixing the two would leave the crate
half-`no_std` and unable to say so.

**A procedural macro with a nicer syntax.** Rejected: it needs `syn` and `quote`, and the project's
dependency rule is that a dependency must do something the standard library cannot. `macro_rules!`
does this, and `cairn-workload` having no dependencies is worth more than a prettier attribute.

**Generate the workload with `cargo generate` or a `cairn new` subcommand.** Rejected for now. A
directory to copy is understood by everyone and needs no tool, and the template's real content is
its comments — which a generator would strip or template away. Worth revisiting if the template
grows options.

**Have `check` fix things.** Rejected. It reports and explains; a tool that edited an author's
build configuration would be doing the one thing this gate must never do, which is decide on bytes
nobody submitted.
