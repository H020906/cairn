# A Cairn workload

Copy this directory, rename the package, write your `run`.

```bash
cp -r workloads/template my-workload
cargo build --release --manifest-path my-workload/Cargo.toml
cargo run -p cairn-worker -- check \
  my-workload/target/wasm32-unknown-unknown/release/my_workload.wasm
```

`check` writes nothing and executes nothing. It says whether Cairn would take the module, and when
the answer is no it prints the fix rather than only the rule.

## What you edit

`src/lib.rs`, and nothing else:

```rust
fn run(input: &[u8], answer: &mut cairn_workload::Answer<'_>) {
    let total: u64 = input.iter().map(|byte| u64::from(*byte)).sum();
    answer.push(&total.to_le_bytes());
}
```

The buffer sizes above it are yours to change. Everything else in this directory is load-bearing
and commented where it is not obvious.

## What you do not edit, and why

**`.cargo/config.toml`.** Three link flags. Without them the module is refused, and the errors you
get on the way to working that out name everything except the cause — the file's own comments have
the measurement, one step at a time. If your workload needs more memory, raise `--initial-memory`
and `--max-memory` together and leave the stack alone.

**`panic = "abort"` in `Cargo.toml`.** A panic becomes a trap. That is a *legitimate answer*, not a
crash: it is deterministic, so every honest volunteer running this module on this input stops at
the same instruction and the network agrees about it.

**`opt-level = "s"`.** Not about speed. It stops the optimiser vectorising loops, which is the
usual way a workload accidentally acquires SIMD and stops being admissible.

## The three rules underneath all of this

**Nothing may differ between two machines.** There is no clock, no entropy, no filesystem and no
network — absent, not restricted. Cairn has no notion of *nearly the same*: two volunteers who
disagree are in a dispute, and a dispute convicts one of them.

**Do not use `f64::sin`, `exp`, `log`, or anything like them.** WebAssembly does not have them, so
they come from whatever math library the host was built with — and those libraries disagree with
each other on every function that has ever been measured here, sometimes by being simply wrong.
Use `cairn_math`, which is a dependency already. `+ - * /` and `sqrt` are specified exactly and are
safe to use directly.

**There is no allocator.** Buffers are the fixed sizes you declare. Overflowing one traps rather
than truncating, deliberately: a short answer that looks like an answer is worse than no answer.

Full contract: [`docs/WORKLOADS.md`](../../docs/WORKLOADS.md).
