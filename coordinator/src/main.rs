//! The Cairn coordinator: dispatch units, collect results, settle the disagreements.
//!
//! ```bash
//! cargo run -p cairn-coordinator -- \
//!   workloads/examples/sum-of-squares.wat workloads/examples/input-a.bin
//! ```
//!
//! Then open <http://127.0.0.1:8080> and the browser worker starts doing the units. That is
//! the whole system: a coordinator, a volunteer, and a work queue between them.
//!
//! # Why this is Rust and not the Java that ARCHITECTURE.md describes
//!
//! Because **the referee executes.** Bisection is a pure state machine that any language can
//! drive, but adjudication rebuilds a machine from a state witness and steps it once — that is
//! the execution kernel, called from the coordinator. A Java coordinator would need JNI, a
//! subprocess, or a second implementation of consensus-critical code; the third is
//! unthinkable and the first two buy nothing until there is a database to be transactional
//! about. See [ADR-0010](../../docs/adr/0010-the-referee-executes-so-the-coordinator-is-rust.md).
//!
//! # How a disagreement is settled
//!
//! Two ways, and which one is used depends on the parties rather than on the coordinator's mood.
//!
//! **By bisection**, when both parties declared they can argue. The coordinator asks each of
//! them what state they claim at a step, `log₂(n)` times, then executes **one instruction** from
//! a state a party hands over. Nobody re-runs the unit. See [`cairn_coordinator::dispute`].
//!
//! **By re-execution**, otherwise. Answering a challenge means producing a state root, and no
//! engine outside this repository can — a browser volunteer is fast and blind
//! ([ADR-0005](../../docs/adr/0005-the-fast-path-cannot-snapshot.md)). Challenging one anyway
//! would time it out and convict an honest volunteer for running in a browser, so the referee
//! does the work itself instead. **That is a route, not a gap**; see
//! [ADR-0011](../../docs/adr/0011-a-volunteer-that-cannot-argue-is-not-challenged.md).
//!
//! # What this coordinator is not
//!
//! No database, no reputation, no canaries, and **no penalties** — a verdict distinguishes a
//! proven lie from an abandonment, and ADR-0001 wants those to cost a volunteer very
//! differently, but acting on that needs a reputation store that does not exist yet.

use std::sync::{Arc, Mutex};

use cairn_coordinator::api;
use cairn_coordinator::grid::{self, Grid};
use cairn_coordinator::journal::{Entry, Journal};
use cairn_coordinator::reputation;

const USAGE: &str = "\
cairn-coordinator — dispatch work units to volunteers and settle the disagreements

USAGE
    cairn-coordinator <workload> [input-file ...] [--bind ADDR] [--replicate PERCENT]
                      [--journal FILE] [--canary PERMILLE]

    <workload>     a .wasm binary or .wat text module, validated and instrumented on startup
    [input-file]   one unit per input file; with none, a single unit with empty input
    --bind         default 127.0.0.1:8080
    --replicate    percentage of units given to a second volunteer as a spot check
                   (default 10; 0 disables). ADR-0001 calls this `r`.
    --journal      append every decision to FILE, and replay it on startup. Without it the
                   grid is in memory only and dies with the process.
    --canary       how often a TRUSTED volunteer is handed a unit whose answer is already
                   known, in permille (default 30; ADR-0001 calls this `c`). Volunteers that
                   have not yet earned trust are checked far harder, and that rate is not a
                   flag. `--canary 0` turns checking off for trusted workers entirely.

                   Canaries need corroborated units to copy, and corroboration comes from
                   --replicate. With --replicate 0 there are none, so there are no canaries
                   and nobody ever becomes trusted. See docs/adr/0015.

WHAT IT DOES
    Registers the workload, queues a unit per input, and serves an HTTP API that volunteers
    poll for work. Almost every unit is accepted after a SINGLE execution — that is the
    project's whole claim. A replicated unit whose two answers differ is settled by the
    referee.

    With --journal, the workload and inputs on the command line are only used the FIRST time:
    afterwards the journal is the grid, and restarting picks up where the process died. A unit
    that was mid-argument comes back unassigned rather than resumed — nobody can be convicted
    for a coordinator's crash. See docs/adr/0014.

    It also serves `browser/` at the root, so opening the printed URL is enough to start
    contributing with no install.
";

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args
        .first()
        .is_none_or(|first| first == "-h" || first == "--help")
    {
        print!("{USAGE}");
        return std::process::ExitCode::SUCCESS;
    }

    match run(&args) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<(), String> {
    let mut positional = Vec::new();
    let mut bind = "127.0.0.1:8080".to_owned();
    let mut replicate = grid::DEFAULT_REPLICATION_PERCENT;
    let mut journal_path: Option<String> = None;
    let mut policy = reputation::Policy::default();

    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--bind" => bind = rest.next().ok_or("--bind needs an address")?.clone(),
            "--journal" => {
                journal_path = Some(rest.next().ok_or("--journal needs a path")?.clone());
            }
            "--canary" => {
                policy.canaries_when_trusted = rest
                    .next()
                    .ok_or("--canary needs a rate in permille")?
                    .parse()
                    .map_err(|_| "--canary needs a number of permille")?;
                if policy.canaries_when_trusted > 1000 {
                    return Err("--canary is permille; 1000 is every unit".to_owned());
                }
            }
            "--replicate" => {
                replicate = rest
                    .next()
                    .ok_or("--replicate needs a percentage")?
                    .parse()
                    .map_err(|_| "--replicate needs a number")?;
            }
            other => positional.push(other.to_owned()),
        }
    }

    let (workload_path, inputs) = positional
        .split_first()
        .ok_or("a workload is required; run with --help")?;

    let source =
        std::fs::read(workload_path).map_err(|e| format!("could not read {workload_path}: {e}"))?;

    let mut grid = Grid::new()
        .with_replication(replicate)
        .with_reputation(reputation::Reputation::new(policy));

    // Open the journal before touching the grid, so that a file this build cannot read stops the
    // coordinator rather than being silently replaced by a fresh grid over the top of it.
    let mut journal = match &journal_path {
        None => None,
        Some(path) => {
            let (journal, history) = Journal::open(std::path::Path::new(path))
                .map_err(|e| format!("could not open the journal {path}: {e}"))?;
            let restored = grid
                .restore(&history)
                .map_err(|e| format!("the journal {path} does not describe this build: {e}"))?;

            println!("journal       {path}");
            if history.is_empty() {
                println!("              new — nothing to recover");
            } else {
                println!(
                    "recovered     {} workloads, {} units, {} results, {} already decided",
                    restored.workloads, restored.units, restored.results, restored.decided
                );
                if restored.canaries > 0 {
                    println!(
                        "              {} canary outcomes, so volunteers keep the standing they earned",
                        restored.canaries
                    );
                }
                // Named individually rather than counted, because somebody was in the middle of
                // each of these and the operator should be able to see who.
                for (unit, parties) in &restored.voided {
                    println!(
                        "              unit {unit} was mid-argument between {} and {} — voided                          and queued again; neither is at fault",
                        parties.first().map_or("?", String::as_str),
                        parties.get(1).map_or("?", String::as_str),
                    );
                }
            }
            Some(journal)
        }
    };

    // The command line describes the grid only when the journal did not. Re-registering and
    // re-queueing on every restart would duplicate every unit in the file, which is the
    // straightforward way a "resumable" coordinator quietly does all its work twice.
    if grid.units().is_empty() {
        let id = grid.register(workload_path, &source)?;
        if let Some(journal) = journal.as_mut() {
            journal
                .append(&Entry::Registered {
                    name: workload_path.clone(),
                    source: source.clone(),
                })
                .map_err(|e| format!("could not write to the journal: {e}"))?;
        }

        println!("workload      {workload_path}");
        println!("unit id       {id}");

        let mut queue = |input: Vec<u8>| -> Result<(), String> {
            let unit = grid.submit(&id, input.clone())?;
            let quorum = grid.unit(unit).map_or(1, |u| u.quorum);
            if let Some(journal) = journal.as_mut() {
                journal
                    .append(&Entry::Queued {
                        workload: id.clone(),
                        input,
                        quorum,
                    })
                    .map_err(|e| format!("could not write to the journal: {e}"))?;
            }
            Ok(())
        };

        if inputs.is_empty() {
            queue(Vec::new())?;
        } else {
            for path in inputs {
                let input =
                    std::fs::read(path).map_err(|e| format!("could not read input {path}: {e}"))?;
                queue(input)?;
            }
        }
    } else {
        println!("workload      from the journal; the command line was not used");
    }

    println!("units queued  {}", grid.units().len());
    println!(
        "replication   {replicate}% (ADR-0001's `r`; a replicated unit goes to two volunteers)"
    );
    println!(
        "canaries      {}‰ once a volunteer is trusted, {}‰ until then (ADR-0001's `c`)",
        policy.canaries_when_trusted, policy.canaries_when_not
    );
    if replicate == 0 {
        println!(
            "              NONE will be minted: a canary copies a unit two volunteers agreed              on, and --replicate 0 produces none. See docs/adr/0015."
        );
    }
    println!();

    // Serve the browser worker if it is where it should be, so one command is the whole system.
    // Absent is not an error: the coordinator is useful to a native worker without it.
    let web_root = ["browser", "../browser"]
        .into_iter()
        .find(|candidate| std::path::Path::new(candidate).join("index.html").is_file());

    api::serve(
        Arc::new(Mutex::new(grid)),
        &bind,
        web_root,
        journal.map(|journal| Arc::new(Mutex::new(journal))),
    )
}
