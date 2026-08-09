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
//! three sources are independently tunable, they have to be told apart, and after
//! [ADR-0005](../../docs/adr/0005-the-fast-path-cannot-snapshot.md) they do not even run on
//! the same path:
//!
//! - **NaN canonicalization** — about six instructions per floating-point operation. Runs on
//!   the **honest path**, because it is what makes two honest workers agree at all. After
//!   ADR-0005 it is the only cost most workloads pay, and the only large one any workload
//!   pays.
//! - **Fuel metering** — two instructions per basic block, and what makes an execution
//!   addressable. Runs **only on disputed units** now.
//! - **Snapshots** — hashing dirty pages every `2^k` instructions. Disputed units only, and
//!   tunable by `k`, which no longer trades against honest-path speed.
//!
//! # What these numbers are and are not
//!
//! Instruction counts are exact and machine-independent. **Wall-clock is not, and this
//! benchmark measures how unreliable it is rather than asserting a figure**: several
//! configurations here instrument to byte-identical modules on some workloads, so timing them
//! against each other reads the harness's own error directly. Anything smaller than that error
//! is reported as *not resolved*.
//!
//! # Two engines, because the answer depends on which one you ask
//!
//! Most sections measure Cairn's interpreter, which is the **slow** path — the one that only
//! runs during arbitration. One section measures wasmtime, a real optimising compiler, and it
//! is the section that matters most, because a volunteer runs a compiler.
//!
//! They do not agree. The honest path is free on both. Fuel metering costs 18%–41% in the
//! interpreter and **five to six times** on the compiler, because a host call is cheap next to
//! interpreted arithmetic and expensive next to compiled arithmetic. See
//! [ADR-0007](../../docs/adr/0007-metering-is-a-jit-problem-not-an-interpreter-problem.md).
//!
//! Run with `cargo bench`.

// Indexing is denied crate-wide because an out-of-range access inside the execution kernel
// would corrupt a trace rather than fail loudly. Here the indices are loop counters taken
// modulo the length of the very vector being indexed, in a benchmark whose failure mode is a
// panicking benchmark. The same reasoning covers the `expect`s.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::cast_precision_loss
)]

use std::time::{Duration, Instant};

use cairn_runtime::canon::{self, Canonicalization, Config};
use cairn_runtime::dispute::{self, Replay, Step};
use cairn_runtime::engine::image;
use cairn_runtime::engine::machine::{Limits, Machine};
use cairn_runtime::validate;

/// Times taken per measurement; the fastest is reported.
///
/// The minimum rather than the mean: a benchmark's slow runs are contaminated by scheduling
/// and cache effects that have nothing to do with the code, while its fastest run is the
/// closest available look at what the code costs on its own.
const SAMPLES: usize = 15;

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

/// One module executed under one snapshot interval.
struct Variant {
    module: Vec<u8>,
    interval: u8,
}

/// What one variant did: fastest observed time, plus the exact counts.
#[derive(Clone, Copy)]
struct Run {
    time: Duration,
    steps: u64,
    snapshots: usize,
}

/// Time several configurations against one another, **interleaved**.
///
/// # Why this is not a loop per variant
///
/// The first version of this benchmark timed all seven samples of one configuration and only
/// then moved to the next. That puts CPU frequency drift *inside* the comparison: whichever
/// variant is timed later runs on a hotter, slower core, and the difference is reported as if
/// it were a property of the code.
///
/// It was not a subtle effect. That version measured a **2.7× gap between two configurations
/// that instrument to byte-identical modules** — the same program, timed twice, "differing" by
/// more than the entire result the benchmark exists to report. Interleaving the rounds spreads
/// any drift across every variant equally, and the byte-identity case is now printed as an
/// explicit noise floor rather than being invisible.
fn race(variants: &[Variant]) -> Vec<Run> {
    let mut runs = vec![
        Run {
            time: Duration::MAX,
            steps: 0,
            snapshots: 0,
        };
        variants.len()
    ];

    // One untimed pass, so the first workload measured does not absorb whatever the machine
    // was still finishing when the benchmark started.
    for variant in variants {
        run_once(variant);
    }

    for round in 0..SAMPLES {
        // Rotate the starting point each round. Interleaving alone still leaves a *position*
        // bias — the variant that runs first in a round meets a quieter machine than the one
        // that runs fifth — and on the tightest workload that bias measured +57% between
        // identical bytes. Rotation gives every variant a turn in every position, and since
        // each variant is scored on its own minimum, each is judged on its best turn.
        for offset in 0..variants.len() {
            let i = (round + offset) % variants.len();
            let (elapsed, trace_steps, trace_snapshots) = run_once(&variants[i]);

            runs[i].time = runs[i].time.min(elapsed);
            runs[i].steps = trace_steps;
            runs[i].snapshots = trace_snapshots;
        }
    }
    runs
}

/// Decode and execute one variant once, timing only the execution.
///
/// # Why the image is rebuilt every round rather than decoded once
///
/// Decoding once and reusing the image looks obviously cheaper, and it is — but it freezes
/// each variant's memory layout for the whole benchmark. A tight interpreter loop is sensitive
/// to where its operator table lands relative to cache sets, so a variant that draws an
/// unlucky allocation stays unlucky for all fifteen rounds, and taking the minimum cannot
/// rescue it: there is no lucky round to find. That is not a hypothesis. With images decoded
/// once, two **byte-identical** modules measured 2.1× apart and stayed there across reruns.
/// Rebuilding per round redraws the layout each time, so the minimum has something to find.
fn run_once(variant: &Variant) -> (Duration, u64, usize) {
    let image = image::decode(&variant.module).expect("module should decode");
    let limits = Limits {
        snapshot_interval_log2: variant.interval,
        ..Limits::default()
    };
    let mut machine = Machine::new(&image, Vec::new(), limits).expect("should instantiate");

    let start = Instant::now();
    let trace = machine.run().expect("workload should not trap");
    let elapsed = start.elapsed();

    (elapsed, trace.steps, trace.snapshots.len())
}

/// Host state for the JIT runs, mirroring what the interpreter keeps internally.
#[derive(Default)]
struct Host {
    output: Vec<u8>,
    fuel: u64,
}

/// Execute one module under wasmtime, timing only the call.
///
/// Compilation and instantiation are outside the timer on purpose. Cairn pays those once per
/// work unit, while the figures here are about the per-instruction cost of instrumentation;
/// folding in a fixed startup cost would flatter the instrumented configurations by diluting
/// them.
fn run_once_jit(module: &[u8]) -> Duration {
    use wasmtime::{Caller, Engine, Extern, Linker, Module, Store};

    let engine = Engine::default();
    let module = Module::new(&engine, module).expect("module should compile");
    let mut store = Store::new(&engine, Host::default());
    let mut linker = <Linker<Host>>::new(&engine);

    linker
        .func_wrap(
            "cairn",
            "charge",
            |mut caller: Caller<'_, Host>, instructions: i32| {
                caller.data_mut().fuel += u64::from(instructions as u32);
            },
        )
        .expect("charge should link");
    linker
        .func_wrap(
            "cairn",
            "output",
            |mut caller: Caller<'_, Host>, ptr: i32, len: i32| {
                let mut buffer = vec![0u8; len as u32 as usize];
                let memory = caller
                    .get_export("memory")
                    .and_then(Extern::into_memory)
                    .expect("workload exports its memory");
                if memory
                    .read(&caller, ptr as u32 as usize, &mut buffer)
                    .is_ok()
                {
                    caller.data_mut().output = buffer;
                }
            },
        )
        .expect("output should link");

    let instance = linker
        .instantiate(&mut store, &module)
        .expect("module should instantiate");
    let entry = instance
        .get_typed_func::<(), ()>(&mut store, validate::ENTRY_POINT)
        .expect("workload exports its entry point");

    let start = Instant::now();
    entry
        .call(&mut store, ())
        .expect("workload should not trap");
    start.elapsed()
}

/// Race several modules under the JIT, with the same interleaving and rotation as [`race`].
fn race_jit(modules: &[Vec<u8>]) -> Vec<Duration> {
    let mut best = vec![Duration::MAX; modules.len()];
    for module in modules {
        run_once_jit(module);
    }
    for round in 0..SAMPLES {
        for offset in 0..modules.len() {
            let i = (round + offset) % modules.len();
            best[i] = best[i].min(run_once_jit(&modules[i]));
        }
    }
    best
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
    /// Determinism only: NaN canonicalization, no metering, no snapshots.
    ///
    /// This is what a volunteer actually runs under ADR-0005, where the fast path returns a
    /// result and a trace is produced later only if someone disputes it. Metering and
    /// snapshots move off the honest path entirely, so this — not `full` — is the `s` that
    /// belongs in ADR-0001's formula.
    honest: Duration,
    honest_steps: u64,
    /// Canonicalizing after every NaN-producing operation, with no metering.
    ///
    /// What the honest path cost between ADR-0005 and ADR-0006 — kept as the comparison that
    /// shows what narrowing canonicalization to escape sites actually bought.
    everywhere: Duration,
    everywhere_steps: u64,
    /// The harness's own error, when it can be read directly.
    ///
    /// `Some` when the determinism-only module and the bare module came out byte-identical —
    /// which happens whenever the workload has no floating-point arithmetic to canonicalize.
    /// Two runs of the same bytes must cost the same, so whatever difference is measured is
    /// entirely the harness. Nothing smaller than this is a result.
    noise: Option<f64>,
}

/// Above this much self-measured error, a workload's wall-clock says nothing about the code.
///
/// Below it, a figure that falls inside the noise is still informative — it means the effect
/// is too small to see, which is a result. Above it, the instrument itself has failed and
/// there is nothing to interpret. Conflating those two would report a broken measurement and a
/// genuine near-zero in the same words.
const RESOLVABLE_NOISE: f64 = 0.10;

impl Measurement {
    /// Whether this workload's wall-clock can be believed at all on this machine.
    fn usable(&self) -> bool {
        self.noise
            .is_none_or(|noise| noise.abs() <= RESOLVABLE_NOISE)
    }

    /// Render a measured overhead, or say why it cannot be rendered.
    fn resolved(&self, value: f64) -> String {
        match self.noise {
            Some(noise) if noise.abs() > RESOLVABLE_NOISE => "not resolved".to_owned(),
            Some(noise) if value.abs() <= noise.abs() => {
                format!("≈0% (±{:.0}%)", noise.abs() * 100.0)
            }
            _ => format!("{:+.0}%", value * 100.0),
        }
    }

    /// Overhead of the fully instrumented module: what a disputing worker pays.
    fn overhead(&self) -> f64 {
        ratio(self.full, self.bare) - 1.0
    }

    /// Overhead of the determinism-only module: what every honest worker pays.
    fn honest_overhead(&self) -> f64 {
        ratio(self.honest, self.bare) - 1.0
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
            canonicalize: Canonicalization::Never,
        },
    );
    let metered_module = canonical(
        source,
        Config {
            meter_fuel: true,
            canonicalize: Canonicalization::Never,
        },
    );
    let full_module = canonical(source, Config::default());
    let honest_module = canonical(source, Config::honest_path());
    // What the honest path would have cost under ADR-0005 alone, before ADR-0006 narrowed
    // canonicalization to escape sites. Kept so the saving is visible rather than asserted.
    let everywhere_module = canonical(
        source,
        Config {
            meter_fuel: false,
            canonicalize: Canonicalization::Everywhere,
        },
    );

    // Read before the modules are moved: when a workload has no floating-point arithmetic,
    // these two configurations produce the same bytes, and the measured gap between them is
    // the harness talking about itself.
    let honest_is_bare = honest_module == bare_module;

    // Without metering there are no `charge` calls, so no snapshots can fire whatever the
    // interval is set to.
    let variants = [
        Variant {
            module: bare_module,
            interval: NO_SNAPSHOTS,
        },
        Variant {
            module: metered_module.clone(),
            interval: NO_SNAPSHOTS,
        },
        Variant {
            module: metered_module,
            interval: DEFAULT_SNAPSHOT_INTERVAL,
        },
        Variant {
            module: full_module,
            interval: DEFAULT_SNAPSHOT_INTERVAL,
        },
        Variant {
            module: honest_module,
            interval: NO_SNAPSHOTS,
        },
        Variant {
            module: everywhere_module,
            interval: NO_SNAPSHOTS,
        },
    ];
    let runs = race(&variants);

    Measurement {
        name,
        bare: runs[0].time,
        bare_steps: runs[0].steps,
        metered: runs[1].time,
        metered_steps: runs[1].steps,
        snapshotted: runs[2].time,
        snapshots: runs[2].snapshots,
        full: runs[3].time,
        full_steps: runs[3].steps,
        honest: runs[4].time,
        honest_steps: runs[4].steps,
        everywhere: runs[5].time,
        everywhere_steps: runs[5].steps,
        noise: honest_is_bare.then(|| ratio(runs[4].time, runs[0].time) - 1.0),
    }
}

// --- workloads ---------------------------------------------------------------------------
//
// Four shapes, chosen because they stress different parts of the instrumentation. A single
// workload would hide the fact that the overhead is wildly uneven across them.

/// Every workload, so the interpreter and JIT sections cannot drift apart.
const WORKLOADS: [(&str, &str); 4] = [
    ("integer loop", INTEGER_LOOP),
    ("float kernel", FLOAT_KERNEL),
    ("memory sweep", MEMORY_SWEEP),
    ("recursion", RECURSIVE),
];

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
        "**Instruction counts are exact, reproducible, and machine-independent. Wall-clock \
         figures are not**, and this document measures how much they are not rather than \
         asserting an error bar — see *Noise floor*. Any wall-clock figure smaller than its \
         workload's noise is printed as *not resolved* instead of as a result.\n"
    );
    println!(
        "Times are the fastest of {SAMPLES} interleaved runs on one machine. Unless a section \
         says otherwise they use Cairn's own interpreter, which is the **slow** path — the one \
         that only runs during arbitration. *On a JIT rather than the interpreter* measures the \
         same things under wasmtime, and the two do not agree at all about what metering \
         costs.\n"
    );

    let measurements = WORKLOADS.map(|(name, source)| measure(name, source));

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
            "| {} | {:.1?} | {:.2}× | {:.2}× | {:.2}× | **{}** |",
            m.name,
            m.bare,
            ratio(m.metered, m.bare),
            ratio(m.snapshotted, m.metered),
            ratio(m.full, m.snapshotted),
            m.resolved(m.overhead()),
        );
    }
    println!(
        "\nThe three middle columns are shown for decomposition only. Read them against the \
         noise floor below before drawing anything from them — on at least one workload here \
         the harness cannot tell these apart from nothing."
    );

    noise_floor(&measurements);
    on_a_jit(&measurements);

    println!("\n## The two paths, after ADR-0005\n");
    println!(
        "The fast path cannot snapshot, so it runs the determinism-only module and returns a \
         result; the fully instrumented module runs only when a result is disputed. The left \
         column is what every honest worker pays. The right column is what a disputed unit \
         costs, on top of an execution that already happened.\n"
    );
    println!(
        "| workload | honest path (ADR-0006) | honest, canonicalizing everywhere | disputed re-execution |"
    );
    println!("|---|---:|---:|---:|");
    for m in &measurements {
        println!(
            "| {} | **{}** | {} | {} |",
            m.name,
            m.resolved(m.honest_overhead()),
            m.resolved(ratio(m.everywhere, m.bare) - 1.0),
            m.resolved(m.overhead()),
        );
    }
    println!(
        "\nThe middle column is what the honest path cost before ADR-0006 narrowed \
         canonicalization to the few operations that can actually leak a NaN payload. Exact \
         instruction counts against bare, which is where that change is unambiguous: {}.",
        measurements
            .iter()
            .map(|m| format!(
                "**{} {:.2}×** (was {:.2}×)",
                m.name,
                m.honest_steps as f64 / m.bare_steps as f64,
                m.everywhere_steps as f64 / m.bare_steps as f64
            ))
            .collect::<Vec<_>>()
            .join(", ")
    );

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

/// The same overheads, measured on a compiler instead of an interpreter.
///
/// # Why this section is the one that matters most
///
/// Everything else in this document runs on Cairn's interpreter, which is the *slow* path —
/// the one that only ever executes during arbitration. What a volunteer actually runs is a
/// JIT, and ADR-0004 was explicit that its figures were "neither an upper nor a lower bound"
/// because of it, guessing that metering would get worse and canonicalization much better.
///
/// wasmtime is not a browser engine, but it is a real optimising compiler, so this is the
/// first evidence about that path rather than speculation about it.
fn on_a_jit(measurements: &[Measurement]) {
    println!("\n## On a JIT rather than the interpreter\n");
    println!(
        "wasmtime, compiling through Cranelift. Compilation and instantiation are outside the \
         timer; only the call to `cairn_run` is measured. This is the closest available look at \
         what a volunteer's own engine would pay — every other figure in this document is the \
         interpreter.\n"
    );
    println!(
        "| workload | honest path, JIT | honest path, interpreter | **interpreter ÷ JIT** | full instrumentation, JIT |"
    );
    println!("|---|---:|---:|---:|---:|");

    for (index, (name, source)) in WORKLOADS.iter().enumerate() {
        let modules = vec![
            canonical(
                source,
                Config {
                    meter_fuel: false,
                    canonicalize: Canonicalization::Never,
                },
            ),
            canonical(source, Config::honest_path()),
            canonical(source, Config::default()),
        ];
        let times = race_jit(&modules);
        let interpreted = measurements[index].honest;
        println!(
            "| {} | {:.1?} | {:.1?} | **{:.0}×** | {:+.0}% |",
            name,
            times[1],
            interpreted,
            ratio(interpreted, times[1]),
            (ratio(times[2], times[0]) - 1.0) * 100.0,
        );
    }

    println!(
        "\n**The `interpreter ÷ JIT` column is the one that prices a dispute.** A trace \
         commitment covers the operand stack and every frame's locals, which no host engine \
         exposes ([ADR-0005](adr/0005-the-fast-path-cannot-snapshot.md)), so a challenged party \
         cannot produce one on the engine they ran the work with — they must re-execute under \
         Cairn's interpreter. That ratio, not the instrumentation overhead, is what a dispute \
         actually costs them."
    );
    println!(
        "\nThe rightmost column is metering's cost on a compiler, and it is included because it \
         is startling and because it is **not a cost anyone pays**: nothing runs the fully \
         instrumented module on a JIT. See \
         [ADR-0008](adr/0008-a-dispute-costs-an-interpreted-re-execution.md)."
    );
}

/// How much of any figure above is the harness rather than the code.
///
/// A workload with no floating-point arithmetic gets nothing from NaN canonicalization, so its
/// determinism-only module is byte-for-byte the bare one. Timing the same bytes twice must
/// give the same answer; whatever it actually gives is this benchmark's error bar, measured
/// rather than asserted. **No figure in this document smaller than the worst value here means
/// anything.**
fn noise_floor(measurements: &[Measurement]) {
    println!("\n## Noise floor\n");
    println!(
        "Measured, not assumed: these rows compare two configurations that produced identical \
         module bytes, so every difference shown is the harness. Nothing in this document \
         smaller than the largest of them is a result.\n"
    );
    println!("| workload | identical bytes timed twice |");
    println!("|---|---:|");

    let mut worst: f64 = 0.0;
    for m in measurements {
        match m.noise {
            Some(noise) => {
                worst = worst.max(noise.abs());
                println!("| {} | {:+.1}% |", m.name, noise * 100.0);
            }
            None => println!("| {} | — (canonicalization changes this module) |", m.name),
        }
    }
    println!("\n**Error bar: ±{:.0}%.**", worst * 100.0);
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
            canonicalize: Canonicalization::Never,
        },
    );

    let intervals = [10u8, 12, 14, 16, 18, 20];
    let mut variants = vec![Variant {
        module: module.clone(),
        interval: NO_SNAPSHOTS,
    }];
    variants.extend(intervals.map(|interval| Variant {
        module: module.clone(),
        interval,
    }));
    let runs = race(&variants);
    let baseline = runs[0].time;

    println!("| interval | snapshots | cost vs no snapshots |");
    println!("|---:|---:|---:|");
    for (i, k) in intervals.iter().enumerate() {
        println!(
            "| 2^{} | {} | {:.2}× |",
            k,
            runs[i + 1].snapshots,
            ratio(runs[i + 1].time, baseline)
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

    // Only workloads whose signal clears their own noise. A verdict computed from figures the
    // instrument cannot resolve would be arithmetic on nothing.
    let usable: Vec<&Measurement> = measurements.iter().filter(|m| m.usable()).collect();
    let excluded: Vec<&str> = measurements
        .iter()
        .filter(|m| !m.usable())
        .map(|m| m.name)
        .collect();

    if usable.is_empty() {
        println!(
            "No workload's overhead cleared the harness's own noise on this run. There is no \
             verdict to give; re-run on a machine with a stable clock."
        );
        return;
    }

    let worst = usable
        .iter()
        .map(|m| m.honest_overhead())
        .fold(f64::MIN, f64::max);
    let best = usable
        .iter()
        .map(|m| m.honest_overhead())
        .fold(f64::MAX, f64::min);
    let worst_full = usable.iter().map(|m| m.overhead()).fold(f64::MIN, f64::max);

    // The policy dials ADR-0001 assumed. Not measured — chosen.
    let canary = 0.03;
    let replication = 0.10;
    let baseline = 2.0;

    println!(
        "`s` is the honest path's overhead, which after [ADR-0005](adr/0005-the-fast-path-cannot-snapshot.md) \
         is determinism instrumentation alone. It ranges from **{:+.0}%** to **{:+.0}%** across \
         these four shapes. Full instrumentation, which now runs only on a disputed unit, costs \
         up to {:+.0}%.\n",
        best * 100.0,
        worst * 100.0,
        worst_full * 100.0
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
    if !excluded.is_empty() {
        println!(
            "\n**Excluded from this verdict: {}.** On {} the harness's own error exceeded the \
             effect being measured, so there is no number to include. Instruction counts for \
             {} are still exact and appear above.",
            excluded.join(", "),
            if excluded.len() == 1 {
                "that workload"
            } else {
                "those workloads"
            },
            if excluded.len() == 1 { "it" } else { "them" },
        );
    }

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
