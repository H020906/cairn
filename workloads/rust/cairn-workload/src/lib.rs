//! Write a Cairn work unit without writing the parts that get it refused.
//!
//! ```rust,ignore
//! #![no_std]
//!
//! cairn_workload::workload! {
//!     input: 4096,
//!     output: 64,
//!     run: run,
//! }
//!
//! fn run(input: &[u8], answer: &mut cairn_workload::Answer) {
//!     let total: u64 = input.iter().map(|byte| u64::from(*byte)).sum();
//!     answer.push(&total.to_le_bytes());
//! }
//! ```
//!
//! That is the whole of it. No `extern` block, no `#[no_mangle]`, no `static mut`, no panic
//! handler, no pointer casts — and, with the `.cargo/config.toml` from `workloads/template`, no
//! link flags either.
//!
//! # What this crate is actually for
//!
//! Not ergonomics. **Every piece of ceremony it removes is a way to be rejected**, usually by an
//! error message that names something other than the problem. The three link flags are the sharp
//! example, and they were measured rather than guessed at:
//!
//! | What an author would try | What they get |
//! |---|---|
//! | the obvious `rustc --target wasm32-unknown-unknown --crate-type cdylib` | Cairn: `memory must declare a maximum` |
//! | add `--max-memory=131072` | `rust-lld: maximum memory too small, 1114112 bytes needed` |
//! | add `--initial-memory=131072` too | `rust-lld: initial memory too small, 1048584 bytes needed` |
//! | add `-zstack-size=65536` | it works |
//!
//! The number in those messages is Rust's **1 MiB default shadow stack**, laid out first, and
//! **neither message mentions a stack.** An author who reads them literally raises the memory
//! ceiling until the module links, and ships a workload that reserves a megabyte it never uses —
//! which every volunteer then has to hold while running it.
//!
//! # The rules this crate cannot remove, only enforce
//!
//! **There is no allocator, and there must not be one.** Two volunteers whose allocators returned
//! different addresses would compute different answers, and Cairn has no notion of *nearly the
//! same* — it would convict one of them. So buffers are fixed sizes you declare, and
//! [`workload!`] takes them as arguments rather than letting you forget.
//!
//! **There is no clock, no entropy, no filesystem and no network.** Not restricted — absent. The
//! only importable module is `cairn` and the only functions in it are the two below.
//!
//! **A trap is a legitimate answer.** It is deterministic: every honest volunteer running this
//! module on this input traps at the same instruction, so the network agrees on it. That is why
//! overflowing your declared buffers traps here rather than truncating: a short answer that looks
//! like an answer is the failure this project exists to avoid, and a trap is not one.

#![no_std]
#![forbid(missing_docs)]

// Cairn's host interface. Two functions, and there is no third.
//
// `charge` also exists in that module, and importing it is refused at the gate — it is the
// instrumentation pass's metering hook, and a workload that could call it could lie about how
// much it had executed.
//
// Plain comments rather than doc comments: rustdoc does not document `extern` blocks and warns
// if you try.
#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "cairn")]
extern "C" {
    // Copies up to `len` bytes of the unit's input to `ptr` and returns the input's *true*
    // length — so a zero-length call is how you ask how much there is.
    fn input(ptr: *mut u8, len: i32) -> i32;
    // Records `len` bytes at `ptr` as the unit's result. The last call wins.
    fn output(ptr: *const u8, len: i32);
}

// Off wasm this crate still has to compile, so that `cargo check` on a workload package tells an
// author about their own mistakes rather than about the absence of a host. Nothing here can run:
// a Cairn unit only exists inside a Cairn engine.
#[cfg(not(target_arch = "wasm32"))]
unsafe fn input(_: *mut u8, _: i32) -> i32 {
    panic!("a Cairn workload only runs inside a Cairn engine; build it for wasm32-unknown-unknown")
}
#[cfg(not(target_arch = "wasm32"))]
unsafe fn output(_: *const u8, _: i32) {
    panic!("a Cairn workload only runs inside a Cairn engine; build it for wasm32-unknown-unknown")
}

/// How many bytes of input this unit was given.
///
/// Costs one host call and copies nothing, so it is the cheap way to find out whether the input
/// fits before deciding what to do about it.
#[must_use]
pub fn input_len() -> usize {
    // SAFETY: a foreign call, which Rust has no safe spelling for. A length of zero means the
    // host copies nothing, so the null pointer is never read from or written to — which is the
    // documented way to ask for the length alone.
    let length = unsafe { input(core::ptr::null_mut(), 0) };
    length.max(0) as usize
}

/// Copy this unit's input into `buffer`, returning the part that was filled.
///
/// **Traps if the input does not fit.** Silently computing on a prefix would produce a wrong
/// answer that looks like a right one, and a wrong answer is worth less than no answer here: it
/// would be accepted, or it would put an honest volunteer into a dispute it deserves to lose.
/// Call [`input_len`] first if your workload wants to decide for itself.
pub fn read_input(buffer: &mut [u8]) -> &[u8] {
    let available = input_len();
    if available > buffer.len() {
        trap();
    }
    let ptr = buffer.as_mut_ptr();
    let capacity = i32::try_from(buffer.len()).unwrap_or(i32::MAX);
    // SAFETY: a foreign call. The pointer and length describe a live buffer the caller owns, and
    // the host copies at most `capacity` bytes into it.
    let copied = unsafe { input(ptr, capacity) };
    let copied = (copied.max(0) as usize).min(buffer.len());
    buffer.get(..copied).unwrap_or(&[])
}

/// Record `bytes` as this unit's result.
///
/// The last call wins, which is why [`Answer`] exists — it accumulates and writes once, so a
/// workload that answers in pieces does not silently keep only the last piece.
pub fn write_output(bytes: &[u8]) {
    let length = i32::try_from(bytes.len()).unwrap_or(i32::MAX);
    // SAFETY: a foreign call. The pointer and length describe a live slice the caller owns, and
    // the host only reads from it.
    unsafe { output(bytes.as_ptr(), length) };
}

/// Stop this unit, deterministically.
///
/// Every honest volunteer running this module on this input reaches the same instruction and
/// stops there, so a trap is a *result* the network agrees on rather than a failure it has to
/// guess about.
pub fn trap() -> ! {
    #[cfg(target_arch = "wasm32")]
    {
        core::arch::wasm32::unreachable()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        panic!("a Cairn workload trapped")
    }
}

/// The unit's answer, accumulated into a buffer you declared and written once.
///
/// Handed to your `run` function by [`workload!`]. You will not normally construct one.
pub struct Answer<'a> {
    buffer: &'a mut [u8],
    filled: usize,
}

impl<'a> Answer<'a> {
    /// Wrap a buffer. [`workload!`] does this for you.
    pub fn new(buffer: &'a mut [u8]) -> Self {
        Self { buffer, filled: 0 }
    }

    /// Append bytes to the answer.
    ///
    /// **Traps if they do not fit**, rather than writing a shorter answer than you asked for. See
    /// the crate documentation for why a truncated answer is worse than no answer.
    pub fn push(&mut self, bytes: &[u8]) {
        let end = self.filled + bytes.len();
        let Some(slot) = self.buffer.get_mut(self.filled..end) else {
            trap()
        };
        slot.copy_from_slice(bytes);
        self.filled = end;
    }

    /// How many bytes have been appended.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.filled
    }

    /// Whether nothing has been appended. A unit may legitimately answer with nothing.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.filled == 0
    }

    /// Hand the accumulated bytes to the host. [`workload!`] does this for you, once.
    pub fn finish(self) {
        write_output(self.buffer.get(..self.filled).unwrap_or(&[]));
    }
}

/// Define this module's entry point, its buffers, and its panic behaviour.
///
/// ```rust,ignore
/// cairn_workload::workload! {
///     input: 4096,
///     output: 64,
///     run: run,
/// }
/// ```
///
/// `input` and `output` are buffer sizes in bytes. They are **stack** locals, so they are bounded
/// by the shadow stack the linker was given — 64 KiB under the template's `.cargo/config.toml`,
/// and both together must fit inside it with room to spare for your own frames.
///
/// # What it expands to
///
/// A `#[no_mangle] pub extern "C" fn cairn_run()` that reads the input, calls your function, and
/// writes the answer once. That is all — **it does not plant a panic handler**, and the reason is
/// a trap this crate walked into first.
///
/// A `no_std` workload needs a `#[panic_handler]`, so the obvious design has this macro provide
/// one. But [`cairn_math`] cannot be `no_std`: `f64::sqrt`, `floor`, `ceil`, `trunc` and
/// `round_ties_even` are single WebAssembly instructions that live in `std` rather than `core`.
/// So a template that used both got `error[E0152]: found duplicate lang item panic_impl`, naming
/// `std` and this macro and not the actual cause.
///
/// **Use `std` and `panic = "abort"`, which is what the template does.** On
/// `wasm32-unknown-unknown` that costs nothing measurable, and the property you actually want —
/// that nothing comes from the host — is checked directly by reading the module's import list.
/// See ADR-0019. If you are writing a `no_std` workload with no such dependency, add
/// [`abort_on_panic!`] beside this macro.
///
/// [`cairn_math`]: https://github.com/H020906/cairn/tree/main/workloads/rust/cairn-math
#[macro_export]
macro_rules! workload {
    (input: $input:expr, output: $output:expr, run: $run:expr $(,)?) => {
        /// The entry point Cairn calls. Generated by `cairn_workload::workload!`.
        #[no_mangle]
        pub extern "C" fn cairn_run() {
            let mut input_buffer = [0u8; $input];
            let mut output_buffer = [0u8; $output];
            let given = $crate::read_input(&mut input_buffer);
            let mut answer = $crate::Answer::new(&mut output_buffer);
            let run: fn(&[u8], &mut $crate::Answer<'_>) = $run;
            run(given, &mut answer);
            answer.finish();
        }
    };
}

/// Turn a panic into a trap, for a `no_std` workload.
///
/// Only for `no_std`. A `std` workload already has a panic handler, and adding a second one fails
/// with `found duplicate lang item panic_impl` — see [`workload!`], which does not plant one for
/// exactly that reason.
///
/// A trap is a deterministic and therefore legitimate answer: every honest volunteer running this
/// module on this input stops at the same instruction.
#[macro_export]
macro_rules! abort_on_panic {
    () => {
        /// A panic is a trap. Generated by `cairn_workload::abort_on_panic!`.
        #[cfg(target_arch = "wasm32")]
        #[panic_handler]
        fn cairn_workload_panic(_: &core::panic::PanicInfo) -> ! {
            $crate::trap()
        }
    };
}
