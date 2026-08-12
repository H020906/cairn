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

mod capacity;
mod client;
mod host;
mod volunteer;

const USAGE: &str = "\
cairn-worker — run a Cairn work unit, or settle a disagreement about one

USAGE
    cairn-worker check     <module>
    cairn-worker run       <module> [input-file]
    cairn-worker trace     <module> [input-file]
    cairn-worker dispute   <module> <assigned-input> <claimed-input>
    cairn-worker prepare   <module> <output.wasm> [--count-fuel]
    cairn-worker volunteer <coordinator-url> [--name NAME] [--jobs N] [--memory MiB]
                                            [--idle-exit N] [--lie-from STEP]

    <module> is a .wasm binary or a .wat text module.
    An omitted input file means an empty input.

WHAT EACH ONE DOES
    check     Asks whether Cairn would take this module, and says why not. Writes nothing
              and executes nothing. Run it before you submit a workload; the refusals it
              reports are the ones a coordinator would give you, with the fix attached.

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

              It uses every core the machine can spare, under ONE worker name — one machine,
              one vote, whatever it is running on. How many units run at once is worked out
              from memory rather than asked for, because a workload may declare up to 256 MiB
              and a plausible-looking number would take a laptop down. --jobs can lower it.

              --jobs N        donate at most N units at once (default: cores - 1, capped at 32)
              --memory MiB    how much memory this machine can spare. Only Linux can be asked
                              this without unsafe code, so everywhere else it is assumed
                              conservatively and the startup header says so.
              --idle-exit N   stop after N consecutive polls with nothing to do. The arguing
                              thread starts counting only once the unit threads have stopped,
                              and any challenge at all resets it: leaving mid-dispute is
                              abandonment, which loses by default.
              --lie-from S    return a wrong answer AND defend it with corrupted roots from
                              step S. A liar has to lie twice, and this one does: bisection
                              converges on step S and convicts it.
              --wrong-answer  return a wrong answer but replay HONESTLY -- a broken engine
                              rather than a liar. Bisection finds nothing to convict, and the
                              trace both parties agree on names the wrong result instead,
                              because the answer is part of the committed state. Nothing is
                              executed by anybody. See docs/adr/0012.

              Both are for the demonstrations in docs/WALKTHROUGH.md.
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
        ["check", module] => check(module),
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
/// Say whether Cairn would take this module, and if not, what to do about it.
///
/// # Why this is its own command rather than a flag on `prepare`
///
/// `prepare` needs somewhere to write, and needing an output path to ask a yes-or-no question is
/// the kind of friction that stops people asking. A workload author's first question is *"will
/// this be accepted"*, and it should cost one argument and change nothing on disk.
///
/// # Why the refusals carry hints
///
/// Because the refusal alone is frequently not enough to act on, and the measurement that proves
/// it is in `workloads/template/.cargo/config.toml`: an author who has just been told *"memory
/// must declare a maximum"* adds `--max-memory` and is then told *"maximum memory too small,
/// 1114112 bytes needed"* by the linker — a message about a 1 MiB shadow stack that never says
/// the word stack. The fix is a third flag neither message names. A gate that knows the answer
/// and does not say it is choosing to be unhelpful.
fn check(module: &str) -> Result<(), String> {
    let bytes =
        std::fs::read(module).map_err(|e| format!("could not read module {module}: {e}"))?;
    let source = if Path::new(module)
        .extension()
        .is_some_and(|ext| ext == "wat")
    {
        wat::parse_bytes(&bytes)
            .map_err(|e| format!("could not assemble {module}: {e}"))?
            .into_owned()
    } else {
        bytes
    };

    if canon::is_canonical(&source) {
        println!("verdict       already canonical — this module has been through the gate");
        println!("bytes         {}", source.len());
        println!();
        println!("This is what a volunteer is sent, not what an author submits. Checking it says");
        println!("nothing about the module you wrote; check that one instead.");
        return Ok(());
    }

    let facts = match validate::validate_submitted(&source, validate::Limits::default()) {
        Ok(facts) => facts,
        Err(refusal) => {
            println!("verdict       REFUSED");
            println!("why           {refusal}");
            if let Some(hint) = hint_for(&refusal) {
                println!();
                println!("{hint}");
            }
            return Err("this module would not be admitted".to_owned());
        }
    };

    let canonical = canon::instrument(&source, Config::honest_path())
        .map_err(|e| format!("admitted, but could not be instrumented: {e}"))?;

    println!("verdict       admissible");
    println!(
        "bytes         {} submitted, {} canonical",
        source.len(),
        canonical.len()
    );
    println!(
        "memory        {} .. {} pages of 64 KiB",
        facts.memory_pages_min, facts.memory_pages_max
    );
    println!(
        "reads input   {}",
        if facts.imports_input { "yes" } else { "no" }
    );
    println!(
        "writes output {}",
        if facts.imports_output {
            "yes".to_owned()
        } else {
            "NO — this unit can never answer anything".to_owned()
        }
    );
    println!("unit id       {}", blake3::hash(&canonical));

    if !facts.imports_output {
        println!();
        println!("A module that never calls `cairn.output` is admissible and useless: it will be");
        println!("run, it will finish, and its result will be empty. That is reported rather than");
        println!("refused, because an empty answer is a legitimate answer for some workloads.");
    }
    Ok(())
}

/// What to do about a refusal, where knowing the rule is not the same as knowing the fix.
///
/// `None` where the refusal already says everything useful. A hint that repeats the error in
/// other words is worse than no hint, because it trains people to stop reading them.
fn hint_for(refusal: &validate::Rejection) -> Option<String> {
    use validate::Rejection as R;
    let advice = match refusal {
        R::UnboundedMemory => {
            "No toolchain emits a memory maximum unless told to, and adding the obvious flag is \
             not enough. All three of these are needed together:\n\n  \
             -C link-arg=-zstack-size=65536\n  \
             -C link-arg=--initial-memory=131072\n  \
             -C link-arg=--max-memory=131072\n\n\
             With only `--max-memory` the linker answers `maximum memory too small, 1114112 bytes \
             needed`, and with `--initial-memory` too it answers `initial memory too small`. \
             Neither message mentions a stack, and a stack is what they are about: Rust's wasm32 \
             target reserves 1 MiB for a shadow stack and lays it out first. `-zstack-size` is \
             what shrinks it.\n\n\
             `workloads/template/.cargo/config.toml` sets all three in a place you set once."
        }
        R::ForeignImport { module, .. } if module.starts_with("wasi") => {
            "That import comes from WASI, so this was built for `wasm32-wasip1` or similar. Build \
             for `wasm32-unknown-unknown`: a Cairn unit has no operating system under it, and the \
             only importable module is `cairn`."
        }
        R::ForeignImport { .. } => {
            "The only importable module is `cairn`. Something in this workload — often a standard \
             library facility that needs a host, such as a clock or a random number — is reaching \
             outside the sandbox. There is no clock, no entropy, no filesystem and no network, and \
             they are absent rather than restricted."
        }
        R::MissingExport { name } if name == "cairn_run" => {
            "The entry point must be exported under exactly `cairn_run`. In Rust that is \
             `#[no_mangle] pub extern \"C\" fn cairn_run()`, and the crate must be \
             `crate-type = [\"cdylib\"]` — a `bin` produces a module expecting `_start`, which a \
             Cairn unit does not have."
        }
        R::MissingExport { name } if name == "memory" => {
            "The linear memory must be exported as `memory`. `wasm-ld` does that for a `cdylib` \
             already, so this usually means `--no-export-memory` is somewhere in your flags, or \
             the crate type is not `cdylib`."
        }
        R::StartSection => {
            "A start section runs code before `cairn_run`, which would execute unmetered. This \
             usually means the crate type is `bin` rather than `cdylib`, or something is \
             registering a constructor."
        }
        R::Invalid { detail } if detail.contains("SIMD") || detail.contains("simd") => {
            "This module uses SIMD, which is almost always the optimiser vectorising a loop rather \
             than anything you wrote. `opt-level = \"s\"` in `[profile.release]` stops it; the \
             template sets that."
        }
        R::Invalid { detail } if detail.contains("zero byte expected") => {
            "That offset is inside a function body and this is very likely a `call_indirect` \
             encoding — which Cairn now admits, so a toolchain new enough to emit something else \
             may have arrived. See docs/adr/0018."
        }
        R::ReferenceValue { .. } | R::ReferenceInstruction { .. } => {
            "Indirect calls are fine — trait objects, function pointers and `dyn` dispatch all \
             work. What is refused is a function *reference* used as a value: passing one as an \
             argument, storing one in a global, or `table.get`. A reference has no \
             host-independent representation, so no execution trace can commit to one."
        }
        R::MemoryTooLarge { limit, .. } => {
            return Some(format!(
                "Lower `--max-memory`. The ceiling is {limit} pages of 64 KiB, and it is a real \
                 constraint rather than a formality: a volunteer decides how many units it can \
                 run at once by adding up declared maxima, so a workload that asks for more than \
                 it needs is asking every volunteer to run fewer units."
            ))
        }
        _ => return None,
    };
    Some(advice.to_owned())
}

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
    let mut wrong_answer = false;
    let mut jobs = None;
    let mut memory = None;

    let mut rest = flags.iter();
    while let Some(flag) = rest.next() {
        match *flag {
            "--name" => name = (*rest.next().ok_or("--name needs a value")?).to_owned(),
            "--jobs" => {
                let asked: usize = rest
                    .next()
                    .ok_or("--jobs needs a count")?
                    .parse()
                    .map_err(|_| "--jobs needs a number")?;
                if asked == 0 {
                    return Err("--jobs 0 donates nothing; leave the flag off instead".to_owned());
                }
                jobs = Some(asked);
            }
            "--memory" => {
                let mib: u64 = rest
                    .next()
                    .ok_or("--memory needs a size in MiB")?
                    .parse()
                    .map_err(|_| "--memory needs a number of MiB")?;
                memory = Some(mib.saturating_mul(1024 * 1024));
            }
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
            "--wrong-answer" => wrong_answer = true,
            other => return Err(format!("unknown flag {other}")),
        }
    }

    volunteer::serve(&volunteer::Volunteer {
        base,
        name: &name,
        jobs,
        memory,
        lies_from,
        wrong_answer,
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
