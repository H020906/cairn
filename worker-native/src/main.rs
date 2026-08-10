//! A Cairn volunteer, on the command line.
//!
//! This is the smallest thing that exercises the whole idea end to end, and the first thing in
//! the repository a person can actually run. Three commands match the three things a volunteer
//! ever does, and a fourth stands in for the coordinator:
//!
//! - `run` — do a work unit and return the answer. Executes on **wasmtime**, a real compiler,
//!   under [`Config::honest_path`]: determinism instrumentation and nothing else.
//! - `trace` — produce a commitment to how a unit was executed. Executes on **Cairn's
//!   interpreter**, under [`Config::dispute_path`]. This only ever happens because somebody
//!   disagreed.
//! - `dispute` — settle a disagreement between two claimed executions by bisecting to the first
//!   instruction where they differ, then adjudicating it.
//! - `prepare` — write the canonical binary without running it. Not a volunteer's job at all:
//!   instrumentation happens once, at registration, and it is here so that a volunteer who is
//!   not Rust — the browser worker in `browser/` — needs no toolchain to take part.
//!
//! # Why `run` and `trace` use different engines
//!
//! Not an implementation detail — it is the central constraint of the design, and running the
//! two commands back to back is the clearest way to see it.
//!
//! A trace commitment covers the operand stack, every frame's locals, the frame chain and the
//! program counter. No WebAssembly engine exposes any of those to its embedder; they need not
//! survive compilation in a recognisable form. So a volunteer's own engine can execute a unit
//! quickly but cannot say anything about *how* it did it, and the interpreter can say
//! everything but is 37×–142× slower. Cairn pays the fast one always and the slow one almost
//! never.
//!
//! See `docs/adr/0005-the-fast-path-cannot-snapshot.md` and
//! `docs/adr/0008-a-dispute-costs-an-interpreted-re-execution.md`.

use std::fmt::Write as _;
use std::path::Path;
use std::process::ExitCode;
use std::time::Instant;

use cairn_runtime::canon::{self, Config};
use cairn_runtime::dispute::{self, Judgment, Replay, Step};
use cairn_runtime::engine::image;
use cairn_runtime::engine::machine::{Limits, Machine};
use cairn_runtime::validate;

mod client;
mod host;
mod volunteer;

const USAGE: &str = "\
cairn-worker — run a Cairn work unit, or settle a disagreement about one

USAGE
    cairn-worker run       <module> [input-file]
    cairn-worker trace     <module> [input-file]
    cairn-worker dispute   <module> <assigned-input> <claimed-input>
    cairn-worker prepare   <module> <output.wasm> [--count-fuel]
    cairn-worker volunteer <coordinator-url> [--name NAME] [--idle-exit N] [--lie-from STEP]

    <module> is a .wasm binary or a .wat text module.
    An omitted input file means an empty input.

WHAT EACH ONE DOES
    run       Executes the unit on wasmtime under honest-path instrumentation and prints
              the result. This is what a volunteer does, and it is the only command whose
              cost matters.

    prepare   Validates and instruments a submitted module and writes the canonical binary,
              without executing it. This is the coordinator's job, done once at registration
              — every volunteer then runs these exact bytes. It exists as a command because
              a volunteer that is not Rust, such as the browser worker in browser/, cannot
              do it for itself and must not have to.

              --count-fuel adds the exported `cairn_fuel` counter, so whoever runs the unit
              can report how much work it was. Costs a few percent; see docs/adr/0009.

    trace     Executes the unit on Cairn's interpreter under full instrumentation and prints
              a commitment to the execution. Only happens when a result is disputed, and it
              cannot use the fast engine — see the module docs.

    dispute   Treats the two inputs as two parties' executions of one unit, bisects to the
              first instruction where they differ, and adjudicates it.

              <assigned-input> is the work unit as the coordinator assigned it, so it is what
              the verdict is decided against. <claimed-input> stands in for a party who
              returned an execution that is not that one — a liar, or bad hardware. The
              protocol cannot tell those apart and neither can this.

    volunteer Joins a running coordinator: takes units, executes them on wasmtime, reports the
              answers, and — unlike a browser volunteer — answers challenges about them
              afterwards. That is the whole reason this mode exists. Only a party that can
              replay under Cairn's interpreter can be bisected against, so a network with
              arguing volunteers settles a disagreement in ~log2(n) messages rather than by
              re-executing the unit. See docs/adr/0011.

              --idle-exit N  stop after N consecutive polls with nothing to do
              --lie-from S   return a wrong answer AND defend it with corrupted roots from
                             step S. A liar has to lie twice: an honest replay reproduces the
                             truth whatever the volunteer originally returned, so a party that
                             lies only once is not convicted, it is merely wrong. For the demo
                             in docs/WALKTHROUGH.md.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();

    let outcome = match refs.as_slice() {
        [] | ["-h"] | ["--help"] | ["help"] => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        ["run", module] => run(module, None),
        ["run", module, input] => run(module, Some(input)),
        ["trace", module] => trace(module, None),
        ["trace", module, input] => trace(module, Some(input)),
        ["dispute", module, first, second] => settle(module, first, second),
        ["prepare", module, out] => publish(module, out, false),
        ["prepare", module, out, "--count-fuel"] => publish(module, out, true),
        ["volunteer", base, rest @ ..] => enlist(base, rest),
        _ => {
            eprint!("{USAGE}");
            return ExitCode::FAILURE;
        }
    };

    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

/// Read a module and prepare it exactly as a coordinator would, under `config`.
///
/// Assembly, validation and instrumentation in that order. Validation is not optional and not
/// advisory: it is what keeps a workload from reaching a clock, a thread, or anything else two
/// honest volunteers could disagree about.
/// A module already through the pass is run as it is; anything else is prepared first.
///
/// Both kinds are legitimate arguments and telling them apart is not the user's job. What a
/// volunteer is actually sent is the **canonical** binary — `prepare` writes exactly those, and
/// the browser worker runs nothing else — while the example workloads in this repository are
/// submissions. Re-instrumenting a canonical module would meter its own metering, so the
/// distinction has to be made somewhere, and here is cheaper than in a bug report.
fn prepare(path: &str, config: Config) -> Result<Prepared, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("could not read module {path}: {e}"))?;

    let source = if Path::new(path).extension().is_some_and(|ext| ext == "wat") {
        wat::parse_bytes(&bytes)
            .map_err(|e| format!("could not assemble {path}: {e}"))?
            .into_owned()
    } else {
        bytes
    };

    if canon::is_canonical(&source) {
        return Ok(Prepared {
            bytes: source,
            as_received: true,
        });
    }

    validate::validate_submitted(&source, validate::Limits::default())
        .map_err(|e| format!("{path} is not an admissible Cairn workload: {e}"))?;

    Ok(Prepared {
        bytes: canon::instrument(&source, config)
            .map_err(|e| format!("could not instrument {path}: {e}"))?,
        as_received: false,
    })
}

/// A module ready to execute, and whether this tool is the one that made it so.
struct Prepared {
    bytes: Vec<u8>,
    /// The module arrived canonical and was run unchanged.
    ///
    /// Carried rather than discarded because every command prints a line saying what it
    /// executed, and that line would otherwise describe the configuration this tool *would*
    /// have applied instead of the module it actually ran.
    as_received: bool,
}

impl Prepared {
    /// What to print for "instrumented", given what this command would have applied.
    fn describe(&self, would_have_been: &str) -> String {
        if self.as_received {
            "as received — already canonical, not re-instrumented".to_owned()
        } else {
            would_have_been.to_owned()
        }
    }
}

fn read_input(path: Option<&str>) -> Result<Vec<u8>, String> {
    match path {
        None => Ok(Vec::new()),
        Some(path) => std::fs::read(path).map_err(|e| format!("could not read input {path}: {e}")),
    }
}

/// Write the canonical binary a volunteer would be sent, without running it.
///
/// The coordinator's half of the protocol, which is otherwise invisible in this repository:
/// instrumentation happens **once, at registration**, and every volunteer executes the
/// resulting bytes unchanged. That is what makes the module's hash a work unit's identity.
///
/// It exists as a command because the browser worker is JavaScript around the page's own
/// engine — the whole point of [ADR-0005] — so it cannot instrument anything, and requiring it
/// to would put a Rust toolchain between a volunteer and contributing.
///
/// [ADR-0005]: ../../docs/adr/0005-the-fast-path-cannot-snapshot.md
fn publish(module: &str, out: &str, count_fuel: bool) -> Result<(), String> {
    let config = Config {
        meter: if count_fuel {
            canon::Metering::Global
        } else {
            canon::Metering::Off
        },
        ..Config::honest_path()
    };

    let prepared = prepare(module, config)?;
    let digest = blake3::hash(&prepared.bytes);

    std::fs::write(out, &prepared.bytes).map_err(|e| format!("could not write {out}: {e}"))?;

    println!("wrote         {out}");
    println!("bytes         {}", prepared.bytes.len());
    println!(
        "instrumented  {}",
        prepared.describe(if count_fuel {
            "honest path + exported fuel counter"
        } else {
            "honest path — determinism only"
        })
    );
    println!("unit id       {digest}");
    Ok(())
}

/// Do the work and report the answer. The honest path, on a real compiler.
fn run(module: &str, input: Option<&str>) -> Result<(), String> {
    let prepared = prepare(module, Config::honest_path())?;
    let input = read_input(input)?;

    let started = Instant::now();
    let executed = host::execute(&prepared.bytes, &input)?;
    let elapsed = started.elapsed();

    println!("engine        wasmtime (compiled)");
    println!(
        "instrumented  {}",
        prepared.describe("honest path — determinism only")
    );
    println!("time          {elapsed:.1?}");
    if let Some(fuel) = executed.fuel {
        println!("cost          {fuel} instructions");
    }
    println!("result        {} bytes", executed.output.len());
    println!("              {}", hex(&executed.output));
    Ok(())
}

/// Join a running coordinator as a volunteer that can also be a party to a dispute.
fn enlist(base: &str, flags: &[&str]) -> Result<(), String> {
    let mut name = format!("native-{}", std::process::id());
    let mut idle_exit = None;
    let mut lies_from = None;

    let mut rest = flags.iter();
    while let Some(flag) = rest.next() {
        match *flag {
            "--name" => name = (*rest.next().ok_or("--name needs a value")?).to_owned(),
            "--idle-exit" => {
                idle_exit = Some(
                    rest.next()
                        .ok_or("--idle-exit needs a count")?
                        .parse()
                        .map_err(|_| "--idle-exit needs a number")?,
                );
            }
            "--lie-from" => {
                lies_from = Some(
                    rest.next()
                        .ok_or("--lie-from needs a step")?
                        .parse()
                        .map_err(|_| "--lie-from needs a number")?,
                );
            }
            other => return Err(format!("unknown flag {other}")),
        }
    }

    volunteer::serve(&volunteer::Volunteer {
        base,
        name: &name,
        lies_from,
        idle_exit,
    })
}

/// Produce a commitment to how the unit executed. The dispute path, on the interpreter.
fn trace(module: &str, input: Option<&str>) -> Result<(), String> {
    let prepared = prepare(module, Config::dispute_path())?;
    let input = read_input(input)?;

    let decoded = image::decode(&prepared.bytes).map_err(|e| format!("could not decode: {e}"))?;
    let mut machine = Machine::new(&decoded, input, Limits::default())
        .map_err(|e| format!("could not instantiate: {e}"))?;

    let started = Instant::now();
    let trace = machine
        .run()
        .map_err(|trap| format!("execution trapped: {trap}"))?;
    let elapsed = started.elapsed();

    println!("engine        Cairn's interpreter — the fast engine cannot do this");
    println!(
        "instrumented  {}",
        prepared.describe("dispute path — metering and snapshots")
    );
    println!("time          {elapsed:.1?}");
    println!("steps         {}", trace.steps);
    println!("fuel          {}", trace.fuel.get());
    println!("snapshots     {}", trace.snapshots.len());
    println!("initial root  {}", hex(&trace.initial));
    println!("final root    {}", hex(&trace.final_root));
    println!("result        {}", hex(&trace.output));
    Ok(())
}

/// Bisect two disagreeing executions to one instruction, then adjudicate it.
fn settle(module: &str, first_input: &str, second_input: &str) -> Result<(), String> {
    let prepared = prepare(module, Config::dispute_path())?;
    let decoded = image::decode(&prepared.bytes).map_err(|e| format!("could not decode: {e}"))?;
    let first_input = read_input(Some(first_input))?;
    let second_input = read_input(Some(second_input))?;
    // The first input is the work unit as assigned. The coordinator holds it, so it is the
    // ground truth adjudication is decided against.
    let adjudication_input = first_input.clone();

    // The disputed length is the longer of the two, so a party that stopped early shows up as
    // disagreeing from the first step past its end rather than as having no opinion.
    let length = Step::new(longest(&decoded, &first_input)?.max(longest(&decoded, &second_input)?));

    let mut first = Replay::new(&decoded, first_input, Limits::default());
    let mut second = Replay::new(&decoded, second_input, Limits::default());

    let started = Instant::now();
    let verdict = dispute::resolve(&mut first, &mut second, length)
        .map_err(|e| format!("could not settle: {e}"))?;
    let bisected = started.elapsed();

    println!("disputed length   {} instructions", length.get());
    println!("bisection rounds  {}", verdict.rounds);
    println!("divergence        {}", verdict.divergence);
    println!("time to bisect    {bisected:.1?}");
    println!();
    println!("The two agreed entering that instruction and disagreed leaving it:");
    println!("  before          {}", opt_hex(verdict.agreed_root));
    println!("  first claims    {}", opt_hex(verdict.first_claim));
    println!("  second claims   {}", opt_hex(verdict.second_claim));
    println!();

    // Adjudication needs the state itself rather than a hash of it, so the witness comes from
    // an execution replayed to the disputed instruction — and it must be an execution of *the
    // work unit as assigned*, which means the first input. That is what makes this a judgement
    // rather than a third opinion: the coordinator holds the unit's input, so it knows what the
    // instruction was supposed to do.
    //
    // Adjudicating against neither party's input would be a bug worth remembering. It produces
    // a perfectly well-formed `BothWrong` verdict, which reads as "both parties lied" when what
    // actually happened is that the referee judged a different problem.
    let mut machine = Machine::new(&decoded, adjudication_input.clone(), Limits::default())
        .map_err(|e| format!("could not instantiate for adjudication: {e}"))?;
    for _ in 0..verdict.divergence.get() {
        if machine.step().is_err() {
            break;
        }
    }
    let witness = machine.witness_for_next_step();

    let started = Instant::now();
    let judgment = dispute::adjudicate(
        &decoded,
        &verdict,
        &witness,
        &adjudication_input,
        Limits::default(),
    )
    .map_err(|e| format!("could not adjudicate: {e}"))?;
    let adjudicated = started.elapsed();

    println!("Adjudicating that one instruction took {adjudicated:.1?}.");
    match judgment {
        Judgment::Guilty { liar } => println!("Verdict: the {liar} was wrong."),
        Judgment::BothWrong { actual } => println!(
            "Verdict: neither claim matches. The state actually became {}.",
            opt_hex(actual)
        ),
        Judgment::Inconsistent => {
            println!("Verdict: inconsistent — both claims match, so the bisection was wrong.");
        }
    }
    Ok(())
}

/// How many instructions an execution runs before finishing or trapping.
fn longest(decoded: &image::Image<'_>, input: &[u8]) -> Result<u64, String> {
    let mut machine = Machine::new(decoded, input.to_vec(), Limits::default())
        .map_err(|e| format!("could not instantiate: {e}"))?;
    // A trap is a legitimate outcome and its step count is still the length of that execution.
    Ok(match machine.run() {
        Ok(trace) => trace.steps,
        Err(_) => machine.steps(),
    })
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn opt_hex(hash: Option<[u8; 32]>) -> String {
    hash.as_ref().map_or_else(
        || "(execution had ended)".to_owned(),
        |bytes| hex(bytes.as_slice()),
    )
}
