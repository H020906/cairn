//! A Cairn work unit, in Rust.
//!
//! The same computation as `../sum-of-squares.wat`, written the way a real workload author
//! would write one. It exists to keep [`docs/WORKLOADS.md`](../../../docs/WORKLOADS.md)
//! honest: that page tells you which target to use and which link flags to pass, and this is
//! the thing that proves those instructions still work.
//!
//! ```bash
//! cargo build --release --target wasm32-unknown-unknown \
//!   --manifest-path workloads/examples/rust/Cargo.toml
//!
//! cargo run -p cairn-worker -- run \
//!   workloads/examples/rust/target/wasm32-unknown-unknown/release/cairn_workload_example.wasm \
//!   workloads/examples/input-a.bin
//! ```
//!
//! CI builds it and puts it through the admission gate on every push, so if a compiler release
//! starts emitting something Cairn refuses, that is a red build rather than a surprise for the
//! next person who tries.
//!
//! # Why `no_std`
//!
//! Not asceticism — `wasm32-unknown-unknown` has no operating system underneath it, so most of
//! what `std` offers is either absent or a stub. Going without it also removes any chance of
//! accidentally reaching for a clock or an allocator, which a Cairn unit must not have: two
//! volunteers whose allocators returned different addresses would compute different answers.
//!
//! # The three things that are easy to get wrong
//!
//! 1. **The target.** `wasm32-wasi` emits imports from `wasi_snapshot_preview1`, and the only
//!    importable module is `cairn`. The unit is refused at the gate.
//! 2. **The memory maximum.** Cairn requires one, and no toolchain emits one by default. That
//!    is `-C link-arg=--max-memory=N`, and it is in `.cargo/config.toml` next to this file.
//! 3. **Exporting the memory under the name `memory`.** `wasm-ld` does that for a `cdylib`
//!    already; `--no-export-memory` anywhere in your flags will break it.

#![no_std]

use core::panic::PanicInfo;

/// A trap, which is a legitimate outcome of a work unit.
///
/// It is deterministic: every honest volunteer running this module on this input traps at
/// exactly the same instruction, so a trap is a *result* the network can agree on rather than
/// a failure it has to guess about.
#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}

// Cairn's host interface. Two functions a workload may use; `charge` is reserved and importing
// it is refused at the gate. Plain comments rather than doc comments because rustdoc does not
// document extern blocks and warns if you try.
#[link(wasm_import_module = "cairn")]
extern "C" {
    // Copies up to `len` bytes of the unit's input to `ptr`, and returns its *true* length — so
    // a zero-length call is how you size a buffer.
    fn input(ptr: i32, len: i32) -> i32;
    // Records `len` bytes at `ptr` as the unit's result. The last call wins.
    fn output(ptr: i32, len: i32);
}

/// Where the answer is written. A fixed buffer rather than an allocation, because there is no
/// allocator and there should not be one.
static mut RESULT: [u8; 8] = [0; 8];

/// The entry point. Exported under exactly this name, and it is the only thing that runs.
///
/// # Safety
///
/// Called by the host with no arguments on a single thread. The `static mut` is touched from
/// here and nowhere else, and a Cairn unit is single-threaded by definition — the validator
/// refuses threads and shared memory, so there is no second caller to race with.
#[no_mangle]
pub extern "C" fn cairn_run() {
    let mut acc: u64 = 0;
    let mut i: u32 = 0;
    while i < 50_000 {
        acc = acc.wrapping_add(u64::from(i) * u64::from(i));
        i += 1;
    }

    // Ask for zero bytes: the call still reports how many are available, which is enough to
    // make two volunteers given different inputs compute different answers — the situation
    // `cairn-worker dispute` exists to settle.
    let length = unsafe { input(0, 0) };
    acc = acc.wrapping_add(length as u32 as u64);

    unsafe {
        let bytes = &raw mut RESULT;
        (*bytes).copy_from_slice(&acc.to_le_bytes());
        output(bytes.cast::<u8>() as i32, 8);
    }
}
