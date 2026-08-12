//! A Cairn workload built out of the shapes a compiler turns into `call_indirect`.
//!
//! # Why this file exists
//!
//! Until ADR-0018, Cairn refused it. Not for anything it does — for how `rustc` spells the table
//! index of a `call_indirect`, which is a padded five-byte LEB128 where the base specification
//! wants one zero byte. That put a gate between Cairn and most non-trivial compiler output, and
//! nothing noticed, because `math-probe` — the only compiled workload in the repository — happens
//! to contain no indirect call at all.
//!
//! So this is a regression test in the shape of a workload. **If a toolchain release starts
//! emitting something the admission gate refuses, this is what turns red.** It is deliberately
//! made of the constructs a compiler cannot avoid lowering through a function table:
//!
//! - a **trait object**, which is a vtable and an indirect call through it;
//! - a **function pointer** chosen at run time, which cannot be devirtualised;
//! - an array of pointers indexed by a value the compiler cannot see, which defeats the
//!   optimiser's last chance to turn any of this into a direct call.
//!
//! The arithmetic is arbitrary and cheap on purpose. What is being tested is the *shape* of the
//! compiled module, not what it computes — though every engine still has to agree on that too,
//! which is what `runtime/tests/toolchain.rs` checks.
//!
//! # The `unsafe` here
//!
//! Calling a host function is a foreign call and Rust has no safe spelling for one. This is the
//! whole unsafe surface a workload needs, and it is what a workload SDK would encapsulate — see
//! `math-probe/probe.rs`, which says the same thing for the same reason.

#![allow(unsafe_code)]

#[link(wasm_import_module = "cairn")]
extern "C" {
    /// Copies up to `len` bytes of this unit's input to `ptr`, and returns its true length.
    fn input(ptr: *mut u8, len: i32) -> i32;
    /// Records `len` bytes at `ptr` as this unit's result.
    fn output(ptr: *const u8, len: i32);
}

/// A step in the pipeline. `dyn Step` is the point: it compiles to a vtable.
trait Step {
    fn apply(&self, x: i64) -> i64;
}

struct Add(i64);
struct Xor(i64);
struct Rotate(u32);

impl Step for Add {
    fn apply(&self, x: i64) -> i64 {
        x.wrapping_add(self.0)
    }
}
impl Step for Xor {
    fn apply(&self, x: i64) -> i64 {
        x ^ self.0
    }
}
impl Step for Rotate {
    fn apply(&self, x: i64) -> i64 {
        x.rotate_left(self.0)
    }
}

/// Plain function pointers, which reach the table by a different route than a vtable does.
const FUNCTIONS: [fn(i64) -> i64; 4] = [
    |x| x.wrapping_mul(6_364_136_223_846_793_005),
    |x| x ^ (x >> 33),
    |x| x.wrapping_add(1_442_695_040_888_963_407),
    |x| x.swap_bytes(),
];

/// How many bytes of input are folded in. Any more are ignored rather than refused, so a caller
/// cannot make the module trap by handing it a long input.
const MAX_INPUT: usize = 256;

#[no_mangle]
pub extern "C" fn cairn_run() {
    let mut buffer = [0u8; MAX_INPUT];
    // SAFETY: a foreign call, which Rust has no safe spelling for. The pointer and length
    // describe a live local buffer, which is the whole of the contract.
    let length = unsafe { input(buffer.as_mut_ptr(), buffer.len() as i32) };
    let count = (length.max(0) as usize).min(buffer.len());

    let steps: [&dyn Step; 3] = [&Add(0x5851_f42d), &Xor(0x1405_7B7E), &Rotate(17)];

    let mut accumulator: i64 = count as i64;
    for (index, byte) in buffer.iter().take(count).enumerate() {
        accumulator = accumulator.wrapping_add(i64::from(*byte));

        // Which step, and which function, depend on the input — so neither call can be resolved
        // at compile time however hard the optimiser tries.
        let step = steps[index % steps.len()];
        accumulator = step.apply(accumulator);

        let function = FUNCTIONS[(accumulator as usize) & 3];
        accumulator = function(accumulator);
    }

    let bytes = accumulator.to_le_bytes();
    // SAFETY: as above.
    unsafe { output(bytes.as_ptr(), bytes.len() as i32) };
}
