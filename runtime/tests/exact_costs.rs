//! The reproducible half of `benches/cost.rs`, turned into a gate.
//!
//! # Why this exists separately from the benchmark
//!
//! `cargo bench` reports two kinds of number. Wall-clock times are not reproducible — the
//! benchmark measures its own error and refuses to report figures smaller than it. Instruction
//! counts, bisection round counts and witness sizes are **exact**: the same input produces the
//! same number on every machine, every run, forever.
//!
//! Only the second kind can be gated, and it is the kind that matters most. Every claim the
//! project actually makes rests on it:
//!
//! - *Instrumentation costs what it costs* — an instruction count.
//! - *Arbitration does not grow with execution length* — a round count.
//! - *A witness is small* — a page count.
//!
//! So the benchmark stays the **reporting** instrument, run by hand, and this is the **gate**,
//! run by CI on every push. A change here is not necessarily a bug — making metering cheaper
//! is *supposed* to move these numbers. It is a change that has to be deliberate, explained,
//! and committed alongside the reasoning, which is exactly what a failing assertion forces.
//!
//! # Why the workloads are smaller than the benchmark's
//!
//! Tests run unoptimised. The benchmark's integer loop is thirty million instructions, which
//! is a fraction of a second in a release build and most of a minute in a debug one. These are
//! the same *shapes* scaled down until the whole file runs in a couple of seconds. Shapes are
//! what these numbers are sensitive to; size only scales them.

#![allow(clippy::expect_used)]

use cairn_runtime::canon::{self, Canonicalization, Config};
use cairn_runtime::dispute::{self, Replay, Step};
use cairn_runtime::engine::image;
use cairn_runtime::engine::machine::{Limits, Machine};
use cairn_runtime::validate;

/// Instructions executed under each instrumentation setting.
#[derive(Debug, PartialEq, Eq)]
struct Counts {
    bare: u64,
    honest: u64,
    metered: u64,
    full: u64,
}

fn instrument(text: &str, config: Config) -> Vec<u8> {
    let source = wat::parse_str(text).expect("workload should assemble");
    validate::validate_submitted(&source, validate::Limits::default())
        .expect("workload should be a valid Cairn module");
    canon::instrument(&source, config).expect("instrumentation should succeed")
}

fn steps(text: &str, config: Config) -> u64 {
    let module = instrument(text, config);
    let decoded = image::decode(&module).expect("should decode");
    let mut machine =
        Machine::new(&decoded, Vec::new(), Limits::default()).expect("should instantiate");
    machine.run().expect("workload should not trap").steps
}

fn counts(text: &str) -> Counts {
    Counts {
        bare: steps(
            text,
            Config {
                meter_fuel: false,
                canonicalize: Canonicalization::Never,
            },
        ),
        honest: steps(text, Config::honest_path()),
        metered: steps(
            text,
            Config {
                meter_fuel: true,
                canonicalize: Canonicalization::Never,
            },
        ),
        full: steps(text, Config::default()),
    }
}

const INTEGER_LOOP: &str = r#"
    (module
      (import "cairn" "output" (func $output (param i32 i32)))
      (memory (export "memory") 1 4)
      (func (export "cairn_run") (local $i i32) (local $acc i32)
        (block $done
          (loop $again
            (br_if $done (i32.ge_u (local.get $i) (i32.const 2000)))
            (local.set $acc (i32.add (i32.mul (local.get $acc) (i32.const 31))
                                     (local.get $i)))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $again)))
        (i32.store (i32.const 0) (local.get $acc))
        (call $output (i32.const 0) (i32.const 4))))
"#;

const FLOAT_KERNEL: &str = r#"
    (module
      (import "cairn" "output" (func $output (param i32 i32)))
      (memory (export "memory") 1 4)
      (func (export "cairn_run") (local $i i32) (local $x f64) (local $acc f64)
        (local.set $x (f64.const 1.0000001))
        (block $done
          (loop $again
            (br_if $done (i32.ge_u (local.get $i) (i32.const 1000)))
            (local.set $acc
              (f64.add (local.get $acc)
                       (f64.div (f64.mul (local.get $x) (local.get $x))
                                (f64.add (local.get $x) (f64.const 1)))))
            (local.set $x (f64.mul (local.get $x) (f64.const 1.0000001)))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $again)))
        (f64.store (i32.const 0) (local.get $acc))
        (call $output (i32.const 0) (i32.const 8))))
"#;

const RECURSION: &str = r#"
    (module
      (import "cairn" "output" (func $output (param i32 i32)))
      (memory (export "memory") 1 4)
      (func $fib (param $n i32) (result i32)
        (if (result i32) (i32.lt_s (local.get $n) (i32.const 2))
          (then (local.get $n))
          (else (i32.add (call $fib (i32.sub (local.get $n) (i32.const 1)))
                         (call $fib (i32.sub (local.get $n) (i32.const 2)))))))
      (func (export "cairn_run")
        (i32.store (i32.const 0) (call $fib (i32.const 15)))
        (call $output (i32.const 0) (i32.const 4))))
"#;

/// The instruction counts every claim about instrumentation cost is built on.
///
/// **If this fails, read the diff before changing the numbers.** Each ratio below says
/// something the project has argued about in an ADR, and a silent move is how a documented
/// conclusion quietly stops being true:
///
/// - `honest` against `bare` is what a volunteer pays. ADR-0006 brought the float kernel's
///   ratio from 2.30× to 1.00× by canonicalizing only where a NaN payload can escape. **If
///   that ratio rises again, escape-site canonicalization has regressed**, and the honest path
///   is paying for determinism it does not need.
/// - `metered` against `bare` is fuel metering, which after ADR-0005 runs only on a disputed
///   unit.
/// - `full` against `metered` is canonicalization on the dispute path, where it still fires
///   after every NaN-producing operation because a state commitment covers the operand stack.
#[test]
fn instrumentation_costs_exactly_what_it_did() {
    assert_eq!(
        counts(INTEGER_LOOP),
        Counts {
            bare: 30_013,
            honest: 30_013,
            metered: 38_021,
            full: 38_021,
        },
        "integer loop"
    );

    assert_eq!(
        counts(FLOAT_KERNEL),
        Counts {
            bare: 23_015,
            honest: 23_021,
            metered: 27_023,
            full: 57_029,
        },
        "float kernel"
    );

    assert_eq!(
        counts(RECURSION),
        Counts {
            bare: 22_694,
            honest: 22_694,
            metered: 34_534,
            full: 34_534,
        },
        "recursion"
    );
}

/// An integer-only workload must gain **nothing** from either canonicalization setting.
///
/// Stated separately from the table because it is a property rather than a number, and because
/// it is the one an unrelated change is most likely to break: an earlier version of the pass
/// gave every function a scratch local whether or not it contained floating-point arithmetic,
/// which cost a purely integer workload 2.76× under recursion while adding zero instructions.
/// A count-based assertion would not have caught that. This one states the intent.
#[test]
fn integer_workloads_pay_nothing_for_float_determinism() {
    for (name, text) in [("integer loop", INTEGER_LOOP), ("recursion", RECURSION)] {
        let c = counts(text);
        assert_eq!(c.honest, c.bare, "{name}: honest path added instructions");
        assert_eq!(
            c.full, c.metered,
            "{name}: canonicalization added instructions to a module with no float arithmetic"
        );
    }
}

/// Bisection rounds against execution length — the claim ADR-0001 actually rests on.
///
/// The coordinator's work must not grow with the disputed execution. Rounds are `log₂` of the
/// length and nothing else, so a hundredfold longer execution costs about seven more rounds.
/// These are exact: bisection is deterministic and so is the divergence point.
#[test]
fn arbitration_does_not_grow_with_execution_length() {
    let workload = |iterations: u32| {
        format!(
            r#"(module
                 (import "cairn" "input"  (func $input  (param i32 i32) (result i32)))
                 (import "cairn" "output" (func $output (param i32 i32)))
                 (memory (export "memory") 1 4)
                 (func (export "cairn_run") (local $i i32) (local $n i32)
                   (block $done
                     (loop $again
                       (br_if $done (i32.ge_u (local.get $i) (i32.const {iterations})))
                       (i32.store (i32.const 8)
                         (i32.add (i32.load (i32.const 8)) (local.get $i)))
                       (local.set $i (i32.add (local.get $i) (i32.const 1)))
                       (br $again)))
                   (local.set $n (call $input (i32.const 0) (i32.const 0)))
                   (i32.store (i32.const 8)
                     (i32.add (i32.load (i32.const 8)) (local.get $n)))
                   (call $output (i32.const 8) (i32.const 4))))"#
        )
    };

    // (iterations, execution length, bisection rounds)
    // (iterations, execution length, bisection rounds). A hundredfold more instructions costs
    // six more rounds, which is log₂ and nothing else.
    for (iterations, expected_length, expected_rounds) in [
        (100u32, 1_928u64, 11u32),
        (1_000, 19_028, 15),
        (10_000, 190_028, 17),
    ] {
        let module = instrument(&workload(iterations), Config::default());
        let decoded = image::decode(&module).expect("should decode");

        let mut probe =
            Machine::new(&decoded, b"a".to_vec(), Limits::default()).expect("should instantiate");
        let length = probe.run().expect("should not trap").steps;

        let mut first = Replay::new(&decoded, b"a".to_vec(), Limits::default());
        let mut second = Replay::new(&decoded, b"bb".to_vec(), Limits::default());
        let verdict = dispute::resolve(&mut first, &mut second, Step::new(length))
            .expect("the two inputs should disagree");

        println!(
            "{iterations} iterations: {length} instructions, {} rounds",
            verdict.rounds
        );
        assert_eq!(length, expected_length, "{iterations} iterations");
        assert_eq!(
            verdict.rounds, expected_rounds,
            "{iterations} iterations: {length} instructions settled in {} rounds",
            verdict.rounds
        );
        assert!(
            f64::from(verdict.rounds) <= (length as f64).log2() + 2.0,
            "rounds outran log2(length), so bisection is not halving"
        );
    }
}

/// A witness stays small, and this pins how small.
///
/// This is the other half of what makes arbitration cheap: the coordinator's work is bounded
/// by the witness, not by the execution length.
///
/// **The bound is not one page, and this workload is the reason to say so.** `cargo bench`
/// reports a worst case of one page, which is true of its workloads and misleading in
/// general — none of them use `memory.fill`. A fill spanning 100,000 bytes touches two pages
/// in a single instruction, and a larger one would touch more. ADR-0001 says exactly this in
/// prose, having previously claimed `O(1)` and been corrected; the number here is that
/// correction made executable.
///
/// So the assertion is a specific small number rather than a principle. If it rises, either
/// the workload changed or something now reaches further in one instruction than it used to,
/// and the coordinator's cost bound needs restating either way.
#[test]
fn a_witness_stays_small() {
    const WORKLOAD: &str = r#"
        (module
          (import "cairn" "output" (func $output (param i32 i32)))
          (memory (export "memory") 8 8)
          (func (export "cairn_run") (local $i i32)
            (memory.fill (i32.const 0) (i32.const 0xab) (i32.const 100000))
            (block $done
              (loop $again
                (br_if $done (i32.ge_u (local.get $i) (i32.const 2000)))
                (i32.store (i32.mul (local.get $i) (i32.const 64)) (local.get $i))
                (local.set $i (i32.add (local.get $i) (i32.const 1)))
                (br $again)))
            (call $output (i32.const 0) (i32.const 4))))
    "#;

    let module = instrument(WORKLOAD, Config::default());
    let decoded = image::decode(&module).expect("should decode");
    let mut machine =
        Machine::new(&decoded, Vec::new(), Limits::default()).expect("should instantiate");

    let mut widest = 0usize;
    let mut deepest = 0usize;
    for _ in 0..20_000 {
        let witness = machine.witness_for_next_step();
        widest = widest.max(witness.pages.len());
        deepest = deepest.max(witness.operand_stack.len());
        if machine.step().is_err() || machine.is_finished() {
            break;
        }
    }

    assert_eq!(
        widest, 2,
        "the widest witness was {widest} pages — a `memory.fill` across 100,000 bytes reaches \
         two, and nothing here should reach further"
    );
    assert!(
        deepest <= 8,
        "operand stack reached {deepest} entries — not wrong, but the witness-size claim was \
         written against single digits and should be restated if this grows"
    );
}
