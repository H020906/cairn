//! A Cairn volunteer, on the command line.
//!
//! This is the smallest thing that exercises the whole idea end to end, and the first thing in
//! the repository a person can actually run. Three commands, matching the three things a
//! volunteer ever does:
//!
//! - `run` — do a work unit and return the answer. Executes on **wasmtime**, a real compiler,
//!   under [`Config::honest_path`]: determinism instrumentation and nothing else.
//! - `trace` — produce a commitment to how a unit was executed. Executes on **Cairn's
//!   interpreter**, under [`Config::dispute_path`]. This only ever happens because somebody
//!   disagreed.
//! - `dispute` — settle a disagreement between two claimed executions by bisecting to the first
//!   instruction where they differ, then adjudicating it.
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

mod host;

const USAGE: &str = "\
cairn-worker — run a Cairn work unit, or settle a disagreement about one

USAGE
    cairn-worker run     <module> [input-file]
    cairn-worker trace   <module> [input-file]
    cairn-worker dispute <module> <assigned-input> <claimed-input>

    <module> is a .wasm binary or a .wat text module.
    An omitted input file means an empty input.

WHAT EACH ONE DOES
    run       Executes the unit on wasmtime under honest-path instrumentation and prints
              the result. This is what a volunteer does, and it is the only command whose
              cost matters.

    trace     Executes the unit on Cairn's interpreter under full instrumentation and prints
              a commitment to the execution. Only happens when a result is disputed, and it
              cannot use the fast engine — see the module docs.

    dispute   Treats the two inputs as two parties' executions of one unit, bisects to the
              first instruction where they differ, and adjudicates it.

              <assigned-input> is the work unit as the coordinator assigned it, so it is what
              the verdict is decided against. <claimed-input> stands in for a party who
              returned an execution that is not that one — a liar, or bad hardware. The
              protocol cannot tell those apart and neither can this.
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
fn prepare(path: &str, config: Config) -> Result<Vec<u8>, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("could not read module {path}: {e}"))?;

    let source = if Path::new(path).extension().is_some_and(|ext| ext == "wat") {
        wat::parse_bytes(&bytes)
            .map_err(|e| format!("could not assemble {path}: {e}"))?
            .into_owned()
    } else {
        bytes
    };

    validate::validate_submitted(&source, validate::Limits::default())
        .map_err(|e| format!("{path} is not an admissible Cairn workload: {e}"))?;

    canon::instrument(&source, config).map_err(|e| format!("could not instrument {path}: {e}"))
}

fn read_input(path: Option<&str>) -> Result<Vec<u8>, String> {
    match path {
        None => Ok(Vec::new()),
        Some(path) => std::fs::read(path).map_err(|e| format!("could not read input {path}: {e}")),
    }
}

/// Do the work and report the answer. The honest path, on a real compiler.
fn run(module: &str, input: Option<&str>) -> Result<(), String> {
    let prepared = prepare(module, Config::honest_path())?;
    let input = read_input(input)?;

    let started = Instant::now();
    let output = host::execute(&prepared, &input)?;
    let elapsed = started.elapsed();

    println!("engine        wasmtime (compiled)");
    println!("instrumented  honest path — determinism only");
    println!("time          {elapsed:.1?}");
    println!("result        {} bytes", output.len());
    println!("              {}", hex(&output));
    Ok(())
}

/// Produce a commitment to how the unit executed. The dispute path, on the interpreter.
fn trace(module: &str, input: Option<&str>) -> Result<(), String> {
    let prepared = prepare(module, Config::dispute_path())?;
    let input = read_input(input)?;

    let decoded = image::decode(&prepared).map_err(|e| format!("could not decode: {e}"))?;
    let mut machine = Machine::new(&decoded, input, Limits::default())
        .map_err(|e| format!("could not instantiate: {e}"))?;

    let started = Instant::now();
    let trace = machine
        .run()
        .map_err(|trap| format!("execution trapped: {trap}"))?;
    let elapsed = started.elapsed();

    println!("engine        Cairn's interpreter — the fast engine cannot do this");
    println!("instrumented  dispute path — metering and snapshots");
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
    let decoded = image::decode(&prepared).map_err(|e| format!("could not decode: {e}"))?;
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
