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

const USAGE: &str = "\
cairn-coordinator — dispatch work units to volunteers and settle the disagreements

USAGE
    cairn-coordinator <workload> [input-file ...] [--bind ADDR] [--replicate PERCENT]

    <workload>     a .wasm binary or .wat text module, validated and instrumented on startup
    [input-file]   one unit per input file; with none, a single unit with empty input
    --bind         default 127.0.0.1:8080
    --replicate    percentage of units given to a second volunteer as a spot check
                   (default 10; 0 disables). ADR-0001 calls this `r`.

WHAT IT DOES
    Registers the workload, queues a unit per input, and serves an HTTP API that volunteers
    poll for work. Almost every unit is accepted after a SINGLE execution — that is the
    project's whole claim. A replicated unit whose two answers differ is settled by the
    referee.

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

    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--bind" => bind = rest.next().ok_or("--bind needs an address")?.clone(),
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

    let mut grid = Grid::new().with_replication(replicate);
    let id = grid.register(workload_path, &source)?;

    println!("workload      {workload_path}");
    println!("unit id       {id}");

    if inputs.is_empty() {
        grid.submit(&id, Vec::new())?;
    } else {
        for path in inputs {
            let input =
                std::fs::read(path).map_err(|e| format!("could not read input {path}: {e}"))?;
            grid.submit(&id, input)?;
        }
    }

    println!("units queued  {}", grid.units().len());
    println!(
        "replication   {replicate}% (ADR-0001's `r`; a replicated unit goes to two volunteers)"
    );
    println!();

    // Serve the browser worker if it is where it should be, so one command is the whole system.
    // Absent is not an error: the coordinator is useful to a native worker without it.
    let web_root = ["browser", "../browser"]
        .into_iter()
        .find(|candidate| std::path::Path::new(candidate).join("index.html").is_file());

    api::serve(Arc::new(Mutex::new(grid)), &bind, web_root)
}
