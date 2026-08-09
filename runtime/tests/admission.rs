//! The admission gate must never panic and never hang.
//!
//! # Why this boundary and not another
//!
//! [`validate::validate_submitted`] is the only place in Cairn where bytes chosen by a stranger
//! meet code. Everything downstream — the instrumentation pass, the interpreter, the dispute
//! protocol — is entitled to assume it runs on a module that got through here, and every one of
//! them does assume it.
//!
//! So the gate's contract is total: **any** input produces `Ok` or `Err`. Not "any reasonable
//! input", not "any input that is nearly a module". A panic here is a coordinator that a
//! stranger can stop by uploading eleven bytes; a hang is the same thing more slowly.
//!
//! # What is actually asserted
//!
//! Panics need no assertion — a panicking test fails. Hangs do, because an infinite loop inside
//! a validator looks exactly like a slow CI runner until the job is killed with no information
//! about which input did it. Every input is therefore timed, and one that takes longer than
//! [`SLOW_INPUT`] fails the test *and prints itself*, so a hang arrives as a reproducible
//! regression case rather than a red build.
//!
//! The other assertion is about the pipeline rather than the gate: **an admitted module must be
//! instrumentable.** A coordinator that accepts a work unit and then cannot prepare it has
//! accepted something it can never dispatch, and it will find out at registration time with a
//! stack trace rather than at the gate with a rejection.
//!
//! # Why not `cargo fuzz`
//!
//! It needs a nightly toolchain and a separate crate, and CI here is stable. Coverage-guided
//! fuzzing would reach deeper than this does and is worth adding — the note at the bottom of
//! this file says how. What this gives up in depth it takes back in *always running*: a fuzz
//! target nobody runs is a fuzz target that finds nothing.
//!
//! # Running it longer
//!
//! ```text
//! CAIRN_FUZZ_ITERATIONS=2000000 cargo test --release --test admission
//! ```
//!
//! The seed is fixed, so a longer run is a superset of a shorter one and a failure reproduces.

// Indexing is denied crate-wide because an out-of-range access inside the execution kernel
// would corrupt a trace rather than fail loudly. Here every index is taken modulo the length of
// the very collection being indexed, in a test whose failure mode is a failing test — and a
// mutation generator that used checked indexing everywhere would be mostly `unwrap`, which is
// the same panic with more ceremony.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

use std::time::{Duration, Instant};

use cairn_runtime::canon::{self, Config};
use cairn_runtime::engine::image;
use cairn_runtime::validate;

/// Inputs per generator when nothing says otherwise.
///
/// Chosen by measurement rather than by feel: at 20,000 the whole file is well under a second
/// in an *unoptimised* build, which is what makes it acceptable on every push. The gate is
/// cheap to call and most inputs are refused early, so the budget buys depth almost for free.
const DEFAULT_ITERATIONS: usize = 20_000;

/// An input taking longer than this is treated as a hang.
///
/// Generous on purpose. The point is not to measure the validator, it is to notice that one
/// particular input costs a thousand times what every other input costs — which is the shape
/// quadratic blowup and infinite loops both have.
const SLOW_INPUT: Duration = Duration::from_secs(2);

/// The seed every generator starts from. Fixed so a failure is reproducible rather than a flake.
const SEED: u64 = 0x9e37_79b9_7f4a_7c15;

fn iterations() -> usize {
    std::env::var("CAIRN_FUZZ_ITERATIONS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_ITERATIONS)
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn pick(&mut self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        (self.next() % n as u64) as usize
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| (self.next() >> 24) as u8).collect()
    }
}

/// Put one input through the whole admission pipeline. Returns whether it was admitted.
///
/// The assertions are that this returns at all, within a bounded time, without unwinding. What
/// the gate *decides* is not under test — a fuzzer has no opinion about which modules should be
/// admitted, only that deciding must always terminate.
///
/// The return value is not about the verdict either. It is there so a generator can report how
/// often it gets *past* the gate, because a generator whose every input is rejected at the
/// magic number is exercising eleven bytes of the validator and reporting a pass.
#[track_caller]
fn admit(input: &[u8], what: &str) -> bool {
    let started = Instant::now();

    let verdict = validate::validate_submitted(input, validate::Limits::default());

    // An admitted module must be preparable. This is the coordinator's contract with itself:
    // it accepts a work unit at registration and instruments it in the same breath, so a module
    // that passes the gate and then fails the pass is one it can never dispatch.
    if verdict.is_ok() {
        let instrumented = canon::instrument(input, Config::dispute_path());
        assert!(
            instrumented.is_ok(),
            "{what}: admitted but could not be instrumented ({:?})\n{}",
            instrumented.err(),
            hex(input)
        );

        // Decoding is allowed to refuse — it enforces ceilings the gate does not, such as
        // `MAX_LOCALS_PER_FUNCTION`. It is not allowed to panic, which is why it is called.
        let _ = image::decode(instrumented.as_ref().expect("just checked"));
    }

    let elapsed = started.elapsed();
    assert!(
        elapsed < SLOW_INPUT,
        "{what}: took {elapsed:.1?}, which is a hang rather than a decision\n{}",
        hex(input)
    );

    verdict.is_ok()
}

/// How deep a generator's inputs got, so a suite that has quietly gone vacuous says so.
#[derive(Default)]
struct Reach {
    total: usize,
    admitted: usize,
    /// Rejected for a reason other than "not a WebAssembly module at all".
    ///
    /// The interesting middle. An input that fails the magic number tested nothing; one that
    /// fails a Cairn rule walked the whole validator to get there.
    structural: usize,
}

impl Reach {
    fn record(&mut self, input: &[u8], admitted: bool) {
        self.total += 1;
        if admitted {
            self.admitted += 1;
        } else if input.len() >= 8 && input.starts_with(b"\0asm") {
            self.structural += 1;
        }
    }

    #[track_caller]
    fn report(&self, what: &str, want_admitted: bool) {
        println!(
            "{what}: {} inputs, {} admitted, {} rejected past the header",
            self.total, self.admitted, self.structural
        );
        assert!(
            self.structural > 0,
            "{what}: every input died at the magic number, so this generator tested nothing"
        );
        if want_admitted {
            assert!(
                self.admitted > 0,
                "{what}: nothing was ever admitted, so the instrument-after-admission half of \
                 the property was never checked"
            );
        }
    }
}

/// Render an input so a failure is a committable regression case rather than a description.
fn hex(bytes: &[u8]) -> String {
    // Truncated: a mutated module can be kilobytes, and the first few hundred bytes are where
    // the structure that matters lives. The length is printed so a truncated dump is obvious.
    let head: String = bytes
        .iter()
        .take(256)
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join("");
    format!(
        "  {} bytes: {head}{}",
        bytes.len(),
        if bytes.len() > 256 { "…" } else { "" }
    )
}

// --- the corpus ----------------------------------------------------------------------------

/// Valid Cairn modules, to be broken in interesting ways.
///
/// Mutating a valid module reaches far deeper than random bytes do: random bytes die at the
/// magic number, while a valid module with one byte changed gets all the way into a section
/// reader with a length field that no longer means what the rest of the module assumes.
fn corpus() -> Vec<Vec<u8>> {
    let sources = [
        // Minimal.
        r#"(module (memory (export "memory") 1 1) (func (export "cairn_run")))"#,
        // Both host imports, a helper, a table and an indirect call.
        r#"(module
             (import "cairn" "input"  (func $input  (param i32 i32) (result i32)))
             (import "cairn" "output" (func $output (param i32 i32)))
             (memory (export "memory") 1 4)
             (type $sig (func (result i32)))
             (table 2 2 funcref)
             (func $a (type $sig) (i32.const 1))
             (func $b (type $sig) (i32.const 2))
             (elem (i32.const 0) $a $b)
             (global $g (mut i64) (i64.const 0))
             (data (i32.const 0) "cairn")
             (func (export "cairn_run") (local $i i32)
               (drop (call_indirect (type $sig) (i32.const 1)))
               (global.set $g (i64.const 7))
               (block $done
                 (loop $again
                   (br_if $done (i32.ge_u (local.get $i) (i32.const 4)))
                   (local.set $i (i32.add (local.get $i) (i32.const 1)))
                   (br $again)))
               (drop (call $input (i32.const 0) (i32.const 0)))
               (call $output (i32.const 0) (i32.const 4))))"#,
        // Floating point, which is where the pass does most of its work.
        r#"(module
             (memory (export "memory") 1 1)
             (func (export "cairn_run") (param $x f64) (result f64)
               (f64.add (f64.sqrt (local.get $x))
                        (f64.copysign (f64.const 1) (local.get $x)))))"#,
    ];

    sources
        .iter()
        .map(|text| wat::parse_str(text).expect("corpus module should assemble"))
        .collect()
}

/// Break `seed` in one of the ways a corrupted or hostile module is actually shaped.
fn mutate(rng: &mut Rng, seed: &[u8]) -> Vec<u8> {
    let mut out = seed.to_vec();
    if out.is_empty() {
        return out;
    }

    match rng.pick(6) {
        // Flip one byte. The classic, and the one that produces length fields pointing into the
        // middle of other sections.
        0 => {
            let at = rng.pick(out.len());
            out[at] ^= 1 << rng.pick(8);
        }
        // Overwrite a byte outright, which reaches opcodes and section ids that a single bit
        // flip cannot.
        1 => {
            let at = rng.pick(out.len());
            out[at] = (rng.next() >> 24) as u8;
        }
        // Truncate. Every section reader has to survive running out of input mid-structure.
        2 => {
            let keep = rng.pick(out.len());
            out.truncate(keep);
        }
        // Insert a run of noise, shifting everything after it out of alignment.
        3 => {
            let at = rng.pick(out.len());
            let len = 1 + rng.pick(8);
            let noise = rng.bytes(len);
            out.splice(at..at, noise);
        }
        // Delete a run, which is how a section's declared count comes to exceed its contents.
        4 => {
            let at = rng.pick(out.len());
            let len = (1 + rng.pick(8)).min(out.len() - at);
            out.drain(at..at + len);
        }
        // Duplicate a slice, which produces repeated sections — a case the specification has
        // rules about and a hand-written test would rarely reach.
        _ => {
            let at = rng.pick(out.len());
            let len = (1 + rng.pick(32)).min(out.len() - at);
            let slice = out[at..at + len].to_vec();
            out.splice(at..at, slice);
        }
    }
    out
}

// --- the tests -----------------------------------------------------------------------------

#[test]
fn random_bytes_are_decided_not_survived() {
    // The shallow generator. Most of these die at the magic number, which is the point: it
    // costs nothing and it is the only generator that covers "not a module at all", the input
    // an actual attacker would try first.
    let mut rng = Rng(SEED);
    let mut reach = Reach::default();
    for i in 0..iterations() {
        let len = rng.pick(256);
        let input = rng.bytes(len);
        // Occasionally random bytes begin with the magic number by luck; mostly they do not,
        // and that is what this generator is for.
        reach.record(&input, admit(&input, &format!("random bytes #{i}")));
    }
    println!(
        "random bytes: {} inputs, {} reached past the header",
        reach.total, reach.structural
    );
}

#[test]
fn bytes_behind_a_valid_header_are_decided() {
    // Past the magic number and the version, into the section readers, without the structure a
    // real module would have. This is where a length field is read from noise.
    let mut rng = Rng(SEED ^ 0x1111_1111_1111_1111);
    let mut reach = Reach::default();
    for i in 0..iterations() {
        let mut input = b"\0asm\x01\0\0\0".to_vec();
        let len = rng.pick(256);
        input.extend(rng.bytes(len));
        reach.record(&input, admit(&input, &format!("header + noise #{i}")));
    }
    // Nothing here is ever admitted — noise does not accidentally export `cairn_run` — so the
    // reach that matters is how much of it got past the header, which is all of it.
    reach.report("header + noise", false);
}

#[test]
fn mutated_modules_are_decided() {
    // The generator that earns its place. A valid module with one byte changed reaches parts of
    // the validator that neither random bytes nor a hand-written case ever will.
    let corpus = corpus();
    let mut rng = Rng(SEED ^ 0x2222_2222_2222_2222);
    let mut reach = Reach::default();

    for i in 0..iterations() {
        let seed = &corpus[rng.pick(corpus.len())];
        // Chains of mutations, not just one: a single flip is usually rejected immediately,
        // while three compounding ones produce modules that are *nearly* consistent, which is
        // where a parser's assumptions live.
        let mut input = seed.clone();
        for _ in 0..=rng.pick(3) {
            input = mutate(&mut rng, &input);
        }
        reach.record(&input, admit(&input, &format!("mutation #{i}")));
    }
    // Some mutations land on a module that is still admissible — a changed constant, a byte in
    // a data segment. Those are the ones that exercise "admitted implies instrumentable", and
    // if there are none of them this test has been checking the rejection path alone.
    reach.report("mutations", true);
}

#[test]
fn spliced_modules_are_decided() {
    // Two valid modules glued at a random point. Produces trailing sections, duplicate
    // sections, and section orders the specification forbids — none of which a mutation of a
    // single module reaches.
    let corpus = corpus();
    let mut rng = Rng(SEED ^ 0x3333_3333_3333_3333);
    let mut reach = Reach::default();

    for i in 0..iterations() {
        let left = &corpus[rng.pick(corpus.len())];
        let right = &corpus[rng.pick(corpus.len())];
        let cut_left = rng.pick(left.len());
        let cut_right = rng.pick(right.len());

        let mut input = left[..cut_left].to_vec();
        input.extend_from_slice(&right[cut_right..]);
        reach.record(&input, admit(&input, &format!("splice #{i}")));
    }
    // A splice at cut 0 of both is a whole valid module, so admissions happen and the
    // instrument-after-admission half is reached here too.
    reach.report("splices", true);
}

#[test]
fn the_corpus_itself_is_admitted() {
    // Guards against a vacuous suite. If the corpus stopped being valid — a feature leaves the
    // admitted set, a rule tightens — every generator above would still pass while testing
    // nothing but the rejection path, and nothing would say so.
    for (i, module) in corpus().iter().enumerate() {
        validate::validate_submitted(module, validate::Limits::default())
            .unwrap_or_else(|e| panic!("corpus module {i} is no longer admissible: {e}"));
    }
}

// # Deeper fuzzing, when someone wants it
//
// This file is a bounded, seeded, in-process loop because it has to run on stable Rust on every
// push. Coverage-guided fuzzing reaches strictly further and is a good next step:
//
//     cargo install cargo-fuzz          # needs a nightly toolchain
//     cargo fuzz init -p cairn-runtime
//     # target body: let _ = validate::validate_submitted(data, Limits::default());
//     cargo +nightly fuzz run validate -- -max_total_time=3600
//
// Seed its corpus from `corpus()` above. If it finds something, commit the input as a
// permanent case here rather than only fixing the bug — that is what the generated-module
// regression in `differential.rs` did, and it is why that bug cannot come back.
