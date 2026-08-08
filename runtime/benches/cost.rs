//! What Cairn's verification actually costs.
//!
//! # The one number this exists to produce
//!
//! [ADR-0001](../../docs/adr/0001-verification-by-dispute-not-replication.md) argues that
//! Cairn beats replication because its steady-state cost per unit of useful science is
//! `1 + s + c + r`, against BOINC's `≈2.0`. Of those three terms, `c` (canary sampling rate)
//! and `r` (selective replication rate) are **policy dials** — they are chosen, not measured.
//!
//! **`s` is the only one that is a fact about the code**, and until it is measured the whole
//! argument is arithmetic on an unknown. That is what this benchmark determines.
//!
//! # Why it decomposes the overhead
//!
//! A single "instrumentation costs X%" figure would be useless for deciding anything. The
//! three sources are independently tunable and have to be told apart:
//!
//! - **Fuel metering** — two instructions per basic block. Unavoidable; it is what makes an
//!   execution addressable.
//! - **Snapshots** — hashing dirty pages every `2^k` instructions. Tunable by `k`, and traded
//!   against how much replay a disputing worker has to do.
//! - **NaN canonicalization** — about six instructions per floating-point operation. The most
//!   expensive of the three for the workloads Cairn actually targets, and the one whose
//!   necessity is least obvious.
//!
//! # What these numbers are and are not
//!
//! They are wall-clock measurements from **one machine and one interpreter**, plus exact
//! instruction counts that are machine-independent. Cairn's fast path in production is the
//! browser's own engine, which will have a different constant factor. The *ratios* transfer;
//! the absolute times do not.
//!
//! Run with `cargo bench`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::time::{Duration, Instant};

use cairn_runtime::canon::{self, Config};
use cairn_runtime::dispute::{self, Replay, Step};
use cairn_runtime::engine::image;
use cairn_runtime::engine::machine::{Limits, Machine};
use cairn_runtime::validate;

/// Times taken per measurement; the fastest is reported.
///
/// The minimum rather than the mean: a benchmark's slow runs are contaminated by scheduling
/// and cache effects that have nothing to do with the code, while its fastest run is the
/// closest available look at what the code costs on its own.
const SAMPLES: usize = 7;

/// Snapshot interval used as "effectively never", to isolate metering cost from snapshot cost.
const NO_SNAPSHOTS: u8 = 62;

/// The production default, from `.env.example`.
const DEFAULT_SNAPSHOT_INTERVAL: u8 = 16;

fn canonical(text: &str, config: Config) -> Vec<u8> {
    let source = wat::parse_str(text).expect("workload should assemble");
    validate::validate_submitted(&source, validate::Limits::default())
        .expect("workload should be a valid Cairn module");
    canon::instrument(&source, config).expect("instrumentation should succeed")
}

/// Execute once, returning wall-clock time, instructions executed, and snapshots taken.
fn execute(module: &[u8], snapshot_interval_log2: u8) -> (Duration, u64, usize) {
    let image = image::decode(module).expect("module should decode");
    let limits = Limits {
        snapshot_interval_log2,
        ..Limits::default()
    };

    let mut best = Duration::MAX;
    let mut steps = 0;
    let mut snapshots = 0;

    for _ in 0..SAMPLES {
        let mut machine = Machine::new(&image, Vec::new(), limits).expect("should instantiate");
        let start = Instant::now();
        let trace = machine.run().expect("workload should not trap");
        let elapsed = start.elapsed();

        best = best.min(elapsed);
        steps = trace.steps;
        snapshots = trace.snapshots.len();
    }
    (best, steps, snapshots)
}

/// One workload's cost under each configuration.
struct Measurement {
    name: &'static str,
    /// No metering, no canonicalization, no snapshots. What the workload costs by itself.
    bare: Duration,
    bare_steps: u64,
    /// Metering only.
    metered: Duration,
    metered_steps: u64,
    /// Metering plus snapshots at the production interval.
    snapshotted: Duration,
    snapshots: usize,
    /// Everything, including NaN canonicalization.
    full: Duration,
    full_steps: u64,
}

impl Measurement {
    fn overhead(&self) -> f64 {
        ratio(self.full, self.bare) - 1.0
    }
}

fn ratio(a: Duration, b: Duration) -> f64 {
    if b.is_zero() {
        return f64::NAN;
    }
    a.as_secs_f64() / b.as_secs_f64()
}

fn measure(name: &'static str, source: &str) -> Measurement {
    let bare_module = canonical(
        source,
        Config {
            meter_fuel: false,
            canonicalize_nan: false,
        },
    );
    let metered_module = canonical(
        source,
        Config {
            meter_fuel: true,
            canonicalize_nan: false,
        },
    );
    let full_module = canonical(source, Config::default());

    // Without metering there are no `charge` calls, so no snapshots can fire whatever the
    // interval is set to.
    let (bare, bare_steps, _) = execute(&bare_module, NO_SNAPSHOTS);
    let (metered, metered_steps, _) = execute(&metered_module, NO_SNAPSHOTS);
    let (snapshotted, _, snapshots) = execute(&metered_module, DEFAULT_SNAPSHOT_INTERVAL);
    let (full, full_steps, _) = execute(&full_module, DEFAULT_SNAPSHOT_INTERVAL);

    Measurement {
        name,
        bare,
        bare_steps,
        metered,
        metered_steps,
        snapshotted,
        snapshots,
        full,
        full_steps,
    }
}

// --- workloads ---------------------------------------------------------------------------
//
// Four shapes, chosen because they stress different parts of the instrumentation. A single
// workload would hide the fact that the overhead is wildly uneven across them.

/// Integer arithmetic in a tight loop. Many small basic blocks, almost no memory traffic —
/// the worst case for fuel metering, which charges per block.
const INTEGER_LOOP: &str = r#"
    (module
      (import "cairn" "output" (func $output (param i32 i32)))
      (memory (export "memory") 1 4)
      (func (export "cairn_run") (local $i i32) (local $acc i32)
        (block $done
          (loop $again
            (br_if $done (i32.ge_u (local.get $i) (i32.const 2000000)))
            (local.set $acc (i32.add (i32.mul (local.get $acc) (i32.const 31))
                                     (local.get $i)))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $again)))
        (i32.store (i32.const 0) (local.get $acc))
        (call $output (i32.const 0) (i32.const 4))))
"#;

/// Floating-point arithmetic. The shape Cairn actually exists to run, and the one NaN
/// canonicalization taxes — it fires after every arithmetic operation here.
const FLOAT_KERNEL: &str = r#"
    (module
      (import "cairn" "output" (func $output (param i32 i32)))
      (memory (export "memory") 1 4)
      (func (export "cairn_run") (local $i i32) (local $x f64) (local $acc f64)
        (local.set $x (f64.const 1.0000001))
        (block $done
          (loop $again
            (br_if $done (i32.ge_u (local.get $i) (i32.const 500000)))
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

/// Writes across many pages. Snapshots have to rehash every page touched since the last one,
/// so this is where snapshot cost shows up.
const MEMORY_SWEEP: &str = r#"
    (module
      (import "cairn" "output" (func $output (param i32 i32)))
      (memory (export "memory") 64 64)
      (func (export "cairn_run") (local $i i32) (local $pass i32)
        (block $passes
          (loop $next_pass
            (br_if $passes (i32.ge_u (local.get $pass) (i32.const 8)))
            (local.set $i (i32.const 0))
            (block $done
              (loop $again
                (br_if $done (i32.ge_u (local.get $i) (i32.const 4194304)))
                (i32.store (local.get $i) (local.get $i))
                (local.set $i (i32.add (local.get $i) (i32.const 64)))
                (br $again)))
            (local.set $pass (i32.add (local.get $pass) (i32.const 1)))
            (br $next_pass)))
        (call $output (i32.const 0) (i32.const 4))))
"#;

/// Deep recursion. Every call is a frame push and pop, which is what makes a state commitment
/// expensive to take.
const RECURSIVE: &str = r#"
    (module
      (import "cairn" "output" (func $output (param i32 i32)))
      (memory (export "memory") 1 4)
      (func $fib (param $n i32) (result i32)
        (if (result i32) (i32.lt_u (local.get $n) (i32.const 2))
          (then (local.get $n))
          (else (i32.add (call $fib (i32.sub (local.get $n) (i32.const 1)))
                         (call $fib (i32.sub (local.get $n) (i32.const 2)))))))
      (func (export "cairn_run")
        (i32.store (i32.const 0) (call $fib (i32.const 27)))
        (call $output (i32.const 0) (i32.const 4))))
"#;

fn main() {
    println!("# Cairn cost benchmark\n");
    println!(
        "Wall-clock figures are the fastest of {SAMPLES} runs on one machine, using Cairn's own \
         interpreter. Instruction counts are exact and machine-independent. Ratios transfer \
         between machines; absolute times do not.\n"
    );

    let measurements = [
        measure("integer loop", INTEGER_LOOP),
        measure("float kernel", FLOAT_KERNEL),
        measure("memory sweep", MEMORY_SWEEP),
        measure("recursion", RECURSIVE),
    ];

    println!("## Instruction count\n");
    println!("How many more instructions the instrumented module executes.\n");
    println!("| workload | bare | metered | full | metering | canonicalization |");
    println!("|---|---:|---:|---:|---:|---:|");
    for m in &measurements {
        let metering = m.metered_steps as f64 / m.bare_steps as f64;
        let canon = m.full_steps as f64 / m.metered_steps as f64;
        println!(
            "| {} | {} | {} | {} | {:.2}× | {:.2}× |",
            m.name, m.bare_steps, m.metered_steps, m.full_steps, metering, canon
        );
    }

    println!("\n## Wall-clock, decomposed\n");
    println!(
        "Each column is the cost of adding one thing to the one before it. `s` in ADR-0001's \
         formula is the last column.\n"
    );
    println!("| workload | bare | +metering | +snapshots | +canonicalization | **s** |");
    println!("|---|---:|---:|---:|---:|---:|");
    for m in &measurements {
        println!(
            "| {} | {:.1?} | {:.2}× | {:.2}× | {:.2}× | **{:+.0}%** |",
            m.name,
            m.bare,
            ratio(m.metered, m.bare),
            ratio(m.snapshotted, m.metered),
            ratio(m.full, m.snapshotted),
            m.overhead() * 100.0,
        );
    }

    println!("\n## Snapshots taken at the default interval\n");
    println!("| workload | snapshots | instructions per snapshot |");
    println!("|---|---:|---:|");
    for m in &measurements {
        let per = if m.snapshots == 0 {
            0
        } else {
            m.metered_steps / m.snapshots as u64
        };
        println!("| {} | {} | {} |", m.name, m.snapshots, per);
    }

    snapshot_interval_sweep();
    bisection_cost();
    witness_size();
    verdict(&measurements);
}

/// How snapshot cost responds to the interval, on the workload that writes most.
fn snapshot_interval_sweep() {
    println!("\n## Snapshot interval against cost\n");
    println!(
        "Lower `k` means finer pre-committed brackets for bisection and more hashing. This is \
         the dial to turn if `s` is too high.\n"
    );
    let module = canonical(
        MEMORY_SWEEP,
        Config {
            meter_fuel: true,
            canonicalize_nan: false,
        },
    );
    let (baseline, _, _) = execute(&module, NO_SNAPSHOTS);

    println!("| interval | snapshots | cost vs no snapshots |");
    println!("|---:|---:|---:|");
    for k in [10u8, 12, 14, 16, 18, 20] {
        let (elapsed, _, snapshots) = execute(&module, k);
        println!(
            "| 2^{} | {} | {:.2}× |",
            k,
            snapshots,
            ratio(elapsed, baseline)
        );
    }
}

/// What settling a dispute costs, against the length of the execution being disputed.
fn bisection_cost() {
    println!("\n## Dispute cost against execution length\n");
    println!(
        "The claim ADR-0001 rests on: arbitration does not get more expensive as the disputed \
         execution gets longer.\n"
    );

    // The accumulator folds in the input length, so two parties given different inputs
    // genuinely diverge. An earlier version read the input and then ignored it, which made
    // both parties compute the same thing and reported "no dispute" for every row.
    let workload = |iterations: u32| {
        format!(
            r#"(module
                 (import "cairn" "input"  (func $input  (param i32 i32) (result i32)))
                 (import "cairn" "output" (func $output (param i32 i32)))
                 (memory (export "memory") 1 4)
                 (func (export "cairn_run") (local $i i32) (local $n i32)
                   (local.set $n (call $input (i32.const 0) (i32.const 0)))
                   (block $done
                     (loop $again
                       (br_if $done (i32.ge_u (local.get $i) (i32.const {iterations})))
                       (i32.store (i32.const 8)
                         (i32.add (i32.load (i32.const 8))
                                  (i32.mul (local.get $i) (local.get $n))))
                       (local.set $i (i32.add (local.get $i) (i32.const 1)))
                       (br $again)))
                   (call $output (i32.const 8) (i32.const 4))))"#
        )
    };

    println!("| execution length | bisection rounds | log2(length) |");
    println!("|---:|---:|---:|");
    for iterations in [1_000u32, 10_000, 100_000] {
        let module = canonical(&workload(iterations), Config::default());
        let image = image::decode(&module).expect("should decode");

        let mut probe = Machine::new(&image, b"a".to_vec(), Limits::default()).unwrap();
        let length = Step::new(probe.run().unwrap().steps);

        let mut first = Replay::new(&image, b"a".to_vec(), Limits::default());
        let mut second = Replay::new(&image, b"bb".to_vec(), Limits::default());

        match dispute::resolve(&mut first, &mut second, length) {
            Ok(verdict) => println!(
                "| {} | {} | {:.0} |",
                length.get(),
                verdict.rounds,
                (length.get() as f64).log2()
            ),
            Err(e) => println!("| {} | (no dispute: {e}) | |", length.get()),
        }
    }
}

/// How large a witness is, which is what bounds an adjudicator's work.
fn witness_size() {
    println!("\n## Witness size\n");
    println!(
        "An adjudicator's cost is set by this, not by the disputed execution's length. Pages \
         are 64 KiB each and dominate; everything else is tens of values.\n"
    );

    let module = canonical(MEMORY_SWEEP, Config::default());
    let image = image::decode(&module).expect("should decode");
    let mut machine = Machine::new(&image, Vec::new(), Limits::default()).unwrap();

    let mut widest_pages = 0;
    let mut deepest_stack = 0;
    let mut total = 0usize;
    let sampled = 20_000;

    for _ in 0..sampled {
        let witness = machine.witness_for_next_step();
        widest_pages = widest_pages.max(witness.pages.len());
        deepest_stack = deepest_stack.max(witness.operand_stack.len());
        total += witness.pages.len();
        if machine.step().is_err() || machine.is_finished() {
            break;
        }
    }

    println!("| measure | value |");
    println!("|---|---:|");
    println!("| instructions sampled | {sampled} |");
    println!("| most pages one instruction needed | {widest_pages} |");
    println!(
        "| mean pages per instruction | {:.3} |",
        total as f64 / sampled as f64
    );
    println!("| deepest operand stack | {deepest_stack} |");
    println!(
        "| worst-case witness payload | {:.0} KiB |",
        widest_pages as f64 * 64.0
    );
}

/// Put the measured `s` back into ADR-0001's formula and say plainly whether it holds.
fn verdict(measurements: &[Measurement]) {
    println!("\n## Against ADR-0001\n");

    let worst = measurements
        .iter()
        .map(Measurement::overhead)
        .fold(f64::MIN, f64::max);
    let best = measurements
        .iter()
        .map(Measurement::overhead)
        .fold(f64::MAX, f64::min);

    // The policy dials ADR-0001 assumed. Not measured — chosen.
    let canary = 0.03;
    let replication = 0.10;
    let baseline = 2.0;

    println!(
        "Measured `s` ranges from **{:+.0}%** to **{:+.0}%** across these four shapes.\n",
        best * 100.0,
        worst * 100.0
    );
    println!("| scheme | cost multiplier |");
    println!("|---|---:|");
    println!("| BOINC, N = 2 | {baseline:.2}× |");
    println!(
        "| Cairn, best case | {:.2}× |",
        1.0 + best + canary + replication
    );
    println!(
        "| Cairn, worst case | {:.2}× |",
        1.0 + worst + canary + replication
    );
    println!(
        "\nUsing the canary rate ({canary}) and replication rate ({replication}) ADR-0001 \
         assumed. Those two are policy, not measurements — they are chosen, and choosing them \
         differently moves these numbers."
    );

    let worst_total = 1.0 + worst + canary + replication;
    println!(
        "\n**Verdict: ADR-0001 {}** at these settings — worst case {:.2}× against {:.2}×.",
        if worst_total < baseline {
            "holds"
        } else {
            "DOES NOT HOLD"
        },
        worst_total,
        baseline
    );
}
