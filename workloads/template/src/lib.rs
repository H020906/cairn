//! A Cairn work unit. Copy this directory, rename the package, and rewrite `run`.
//!
//! ```bash
//! cargo build --release --manifest-path workloads/template/Cargo.toml
//!
//! cargo run -p cairn-worker -- run \
//!   workloads/template/target/wasm32-unknown-unknown/release/cairn_workload_template.wasm \
//!   workloads/examples/input-a.bin
//! ```
//!
//! `--target` is absent from that command because `.cargo/config.toml` next to this file already
//! says it, along with the three link flags. That file is load-bearing; its comments say why.
//!
//! Before submitting, ask Cairn whether it will take the module:
//!
//! ```bash
//! cargo run -p cairn-worker -- check <your-module.wasm>
//! ```
//!
//! # What a work unit is
//!
//! A pure function from a byte string to a byte string, run once by a stranger's computer, and
//! required to give **bit-identical** results on every machine that runs it. There is no clock,
//! no entropy, no filesystem, no network and no allocator — not restricted, absent. Whatever you
//! compute here, two volunteers must compute the same, because Cairn has no notion of *nearly the
//! same*: a disagreement is a dispute, and arbitration convicts one of the two.
//!
//! # Floating point is allowed, and it is the sharp edge
//!
//! `f64` arithmetic is bit-exact across engines and you should use it. But WebAssembly specifies
//! only `+ - * /` and `sqrt`; it has no `exp`, no `log`, no `sin`. Rust's `f64::sin` on this
//! target comes from whatever math library the host was built with, and those libraries disagree
//! with each other on **every** function that has been measured here — and are sometimes simply
//! wrong. Use [`cairn_math`] instead. See ADR-0016.
//!
//! # Why this is not `no_std`
//!
//! It looks as though it should be, and an earlier draft was. `cairn-math` cannot be: `f64::sqrt`,
//! `floor`, `ceil`, `trunc` and `round_ties_even` are **single WebAssembly instructions** that
//! Rust puts in `std` rather than `core`, because on an ordinary target they are libm calls. Using
//! both a `no_std` crate and `cairn-math` produces `error[E0152]: found duplicate lang item
//! panic_impl`, which names everything except the cause.
//!
//! `std` costs nothing here. `panic = "abort"` removes the unwinding machinery, and the property
//! that actually matters — **that nothing comes from the host** — is not something `no_std`
//! proves anyway. It is checked directly, by reading this module's import section and requiring it
//! to be exactly `cairn.input` and `cairn.output`. See ADR-0019.

/// Input buffer, in bytes. Grow it if your units are larger; it lives on the shadow stack that
/// `.cargo/config.toml` sizes, so `input + output` must fit inside 64 KiB with room for frames.
const INPUT: usize = 4096;

/// Output buffer, in bytes. Appending past it traps rather than truncating, deliberately.
const OUTPUT: usize = 64;

cairn_workload::workload! {
    input: INPUT,
    output: OUTPUT,
    run: run,
}

/// Your work goes here.
///
/// This one sums the input bytes and takes a square root, which is enough to prove the toolchain
/// and the math library are wired up. Replace it.
fn run(input: &[u8], answer: &mut cairn_workload::Answer<'_>) {
    let mut total: u64 = 0;
    for byte in input {
        total = total.wrapping_add(u64::from(*byte));
    }

    // `cairn_math::sqrt` rather than `f64::sqrt` for consistency with the rest of the library,
    // though this one function is specified exactly by IEEE-754 and would agree either way. The
    // functions where that is *not* true are the reason `cairn-math` exists.
    let root = cairn_math::sqrt(total as f64);

    answer.push(&total.to_le_bytes());
    answer.push(&root.to_bits().to_le_bytes());
}
