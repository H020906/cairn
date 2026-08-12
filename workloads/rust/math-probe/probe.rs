//! A Cairn workload that evaluates every `cairn-math` function and reports every bit.
//!
//! This exists to be disagreed with. It is compiled to WebAssembly and run through Cairn's own
//! interpreter, wasmi, wasmtime, and the V8 in a volunteer's browser, and the four are required
//! to produce identical bytes. That is the property `cairn-math` is for; everything else about
//! the crate is in service of it.
//!
//! # Why it is one file built by `rustc` rather than a cargo package
//!
//! `cairn-math` has no dependencies, so there is nothing for a package manager to resolve. Two
//! `rustc` invocations produce the module, which means the tests that need it can build it
//! themselves without nesting one cargo inside another and without a checked-in binary that
//! nobody can regenerate. See `no_host_math_reaches_the_module` in `cairn-math/tests/wasm.rs`.
//!
//! # The `unsafe` here
//!
//! Calling a host function is a foreign call, and Rust has no safe spelling for one. This is
//! the entire unsafe surface a workload needs — three declarations and two static buffers — and
//! it is what a workload SDK would encapsulate so that authors never write it themselves.
//! `cairn-math` itself contains no `unsafe` at all; it declares `#![forbid(unsafe_code)]`.

#![allow(unsafe_code)]

#[link(wasm_import_module = "cairn")]
extern "C" {
    /// Copies up to `len` bytes of this unit's input to `ptr`, and returns its true length.
    fn input(ptr: *mut u8, len: i32) -> i32;
    /// Records `len` bytes at `ptr` as this unit's result.
    fn output(ptr: *const u8, len: i32);
}

/// How many arguments one unit evaluates. Any more are ignored rather than refused, so a
/// caller cannot make the module trap by handing it a long input.
const MAX_ARGUMENTS: usize = 64;
/// How many results each argument produces.
const FUNCTIONS: usize = 26;

/// Every function in the crate, at one argument.
///
/// Two of them need their argument bent into range first: `asin` and `acos` are undefined
/// outside `[-1, 1]`, and mapping through `x/(1+|x|)` covers that interval densely from any
/// input at all. The rest take whatever they are given, including infinities and NaNs, because
/// how those propagate is part of what the engines have to agree about.
fn evaluate(x: f64) -> [f64; FUNCTIONS] {
    use cairn_math as m;
    let magnitude = x.abs();
    let unit = x / (1.0 + magnitude);
    [
        m::exp(x),
        m::exp2(x),
        m::expm1(x),
        m::ln(magnitude),
        m::log2(magnitude),
        m::log10(magnitude),
        m::ln_1p(x),
        m::sin(x),
        m::cos(x),
        m::tan(x),
        m::asin(unit),
        m::acos(unit),
        m::atan(x),
        m::atan2(x, 1.5),
        m::sinh(x),
        m::cosh(x),
        m::tanh(x),
        m::cbrt(x),
        m::sqrt(magnitude),
        m::pow(magnitude, 3.7),
        m::pow(magnitude, x),
        m::hypot(x, 1.5),
        m::fmod(x, 3.25),
        m::floor(x),
        m::ceil(x),
        m::round(x),
    ]
}

/// The entry point Cairn calls.
///
/// The buffers are locals rather than statics. Fourteen kilobytes is nothing against
/// WebAssembly's default stack, and it means the module holds no mutable global state at all —
/// so there is no question about what a second call would see, and nothing for the trace
/// commitment to have an opinion about.
#[no_mangle]
pub extern "C" fn cairn_run() {
    let mut arguments = [0u8; MAX_ARGUMENTS * 8];
    let mut results = [0u8; MAX_ARGUMENTS * FUNCTIONS * 8];

    // SAFETY: a foreign call, which Rust has no safe spelling for. The pointer and length
    // describe a live local buffer, which is the whole of the contract.
    let length = unsafe { input(arguments.as_mut_ptr(), arguments.len() as i32) };
    // A negative or oversized length is the host's business, not this module's; clamp and
    // carry on so that a malformed unit produces a short answer rather than a trap.
    let count = (length.max(0) as usize).min(arguments.len()) / 8;

    let mut written = 0;
    for chunk in arguments.chunks_exact(8).take(count) {
        let bits = u64::from_le_bytes(chunk.try_into().expect("chunks_exact yields eight bytes"));
        for value in evaluate(f64::from_bits(bits)) {
            results[written..written + 8].copy_from_slice(&value.to_bits().to_le_bytes());
            written += 8;
        }
    }
    // SAFETY: as above.
    unsafe { output(results.as_ptr(), written as i32) };
}
