//! The HTTP surface. It translates and decides nothing.
//!
//! Every decision is in [`crate::grid`]. This file parses a request, calls one method, and
//! formats the answer — which is why the coordinator's behaviour can be tested without opening
//! a socket, and why reading `grid.rs` alone tells you what the coordinator does.
//!
//! # The API, in full
//!
//! ```text
//! GET  /api/lease?worker=NAME     take a unit    → {unit, workload, input}  or 204
//! POST /api/result?unit=N&worker=NAME&fuel=F&bisects=1
//!                                 return an answer, body = the output in hex
//! GET  /api/module/{id}           the canonical module a volunteer executes
//! GET  /api/module/{id}?form=dispute   the fully instrumented module the parties replay
//! GET  /api/challenge?worker=NAME what a party to a dispute is being asked  → or 204
//! POST /api/challenge?worker=NAME&token=T        the answer, body depends on the question
//! GET  /api/status                every unit and where it got to
//! GET  /api/disputes              every argument, its transcript, and a machine-readable verdict
//! GET  /                          the browser worker, served from browser/
//! ```
//!
//! # The two challenge endpoints are the interactive protocol
//!
//! They are what makes a dispute cost `O(log n)` messages instead of an execution. A party
//! polls the first, replays to the step it names, and posts the root back to the second.
//! [`crate::dispute`] is on the other side of them, blocked in
//! [`cairn_runtime::dispute::resolve`], which is the same function `cargo run --example
//! dispute` drives against in-process parties. Nothing about the protocol knows it is on a
//! network.
//!
//! # Why the responses are hand-written JSON
//!
//! Five endpoints returning four shapes between them. `serde` and `serde_json` are excellent
//! and would add sixty crates to a dependency tree that currently has four, to save perhaps
//! forty lines. The same reasoning kept `criterion` out of the benchmark and `proptest` out of
//! the bisection tests: **a dependency has to do something the standard library cannot.**
//!
//! The one place that reasoning would fail is *parsing untrusted JSON*, which is fiddly and
//! security-relevant — so this API does not parse any. Inputs arrive as query parameters and
//! raw request bodies.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use tiny_http::{Header, Request, Response, Server};

use cairn_runtime::dispute::Party;

use crate::dispute::{Answer, Conclusion, Dispute, Question};
use crate::grid::{Grid, Outcome, Submission};
use crate::journal::{Entry, Journal};

/// Serve until killed.
///
/// # Errors
///
/// If the address cannot be bound.
pub fn serve(
    grid: Arc<Mutex<Grid>>,
    address: &str,
    web_root: Option<&str>,
    journal: Option<Arc<Mutex<Journal>>>,
) -> Result<(), String> {
    let server = Server::http(address).map_err(|e| format!("could not bind {address}: {e}"))?;

    println!("Cairn coordinator → http://{address}");
    if let Some(root) = web_root {
        println!("  browser worker    http://{address}/   (from {root})");
    }
    println!("  status            http://{address}/api/status");
    println!();

    for request in server.incoming_requests() {
        // A panic while handling one request must not take the coordinator down: a work unit is
        // a program somebody else wrote, and the paths below touch it. `catch_unwind` is not
        // available across the `Request` type cleanly, so the handler is written to return
        // errors rather than unwrap.
        if let Err(e) = handle(&grid, request, web_root, journal.as_ref()) {
            eprintln!("request failed: {e}");
        }
    }
    Ok(())
}

fn handle(
    grid: &Arc<Mutex<Grid>>,
    request: Request,
    web_root: Option<&str>,
    journal: Option<&Arc<Mutex<Journal>>>,
) -> Result<(), String> {
    let url = request.url().to_owned();
    let path = url.split('?').next().unwrap_or("").to_owned();

    match path.as_str() {
        "/api/lease" => lease(grid, request, &url, journal),
        "/api/result" => result(grid, request, &url, journal),
        "/api/challenge" if request.method() == &tiny_http::Method::Post => {
            answer_challenge(grid, request, &url)
        }
        "/api/challenge" => challenge(grid, request, &url),
        "/api/status" => status(grid, request),
        "/api/disputes" => disputes(grid, request),
        p if p.starts_with("/api/module/") => module(grid, request, p, &url),
        _ => match web_root {
            Some(root) => static_file(request, root, &path),
            None => respond(request, 404, "text/plain", b"not found".to_vec()),
        },
    }
}

// --- the journal ------------------------------------------------------------------------------

/// What a result and its outcome mean for the record.
///
/// The result itself always, and the outcome only when the unit stopped being `Open` — an
/// outcome of `Open` means the grid is still waiting for a second opinion, and there is nothing
/// decided to write down.
///
/// `Disputed` records the parties and not the argument. A dispute cannot be resumed from a file
/// and must not be: restarting into a half-finished argument would time out whichever party did
/// not come back and convict a volunteer for the coordinator's crash. See
/// [`crate::journal`].
fn entries_for(grid: &Grid, unit: usize, submission: &Submission, outcome: &Outcome) -> Vec<Entry> {
    let mut entries = vec![Entry::Answered {
        unit,
        worker: submission.worker.clone(),
        output: submission.output.clone(),
        fuel: submission.fuel,
        bisects: submission.bisects,
    }];

    match outcome {
        Outcome::Open => {}
        Outcome::Accepted { output } => entries.push(Entry::Accepted {
            unit,
            output: output.clone(),
        }),
        Outcome::Settled { verdict, output } => entries.push(Entry::Settled {
            unit,
            verdict: verdict.clone(),
            output: output.clone(),
        }),
        // Both names, read back off the dispute the grid just opened. The obvious shortcut —
        // this submission's worker twice — produces a record that looks complete and names one
        // party, which is exactly the shape of thing that goes unnoticed until reputation needs
        // to know whose argument was dropped.
        Outcome::Disputed { dispute } => {
            let parties = grid
                .dispute(*dispute)
                .map(|argument| argument.parties.clone())
                .unwrap_or_else(|| [submission.worker.clone(), String::new()]);
            entries.push(Entry::Disputed { unit, parties });
        }
    }
    entries
}

/// Append entries, reporting a failure without dropping the request.
///
/// A journal that cannot be written is a serious problem and **not** a reason to fail the
/// volunteer's request: the work was done, the grid has accepted it, and refusing the result now
/// would throw away somebody's electricity to protest about a disk. It is reported loudly and
/// the coordinator carries on with a record that is known to be short.
fn record(journal: &Arc<Mutex<Journal>>, entries: &[Entry]) {
    let Ok(mut journal) = journal.lock() else {
        eprintln!("journal lock poisoned; THIS COORDINATOR IS NO LONGER RECOVERABLE");
        return;
    };
    for entry in entries {
        if let Err(e) = journal.append(entry) {
            eprintln!("could not write to the journal: {e}");
            eprintln!("  the grid is correct in memory and will NOT survive a restart intact");
            return;
        }
    }
}

// --- endpoints -------------------------------------------------------------------------------

fn lease(
    grid: &Arc<Mutex<Grid>>,
    request: Request,
    url: &str,
    journal: Option<&Arc<Mutex<Journal>>>,
) -> Result<(), String> {
    let Some(worker) = query(url, "worker") else {
        return respond(request, 400, "text/plain", b"worker= is required".to_vec());
    };

    let assignment = {
        let mut grid = grid.lock().map_err(|_| "grid lock poisoned")?;
        let assignment = grid.lease(&worker, Instant::now());
        // Recorded so that a volunteer which was mid-unit when the coordinator died is still
        // recognised when it comes back with the answer. Not flushed — see `Journal::append`.
        if let (Some(journal), Some(a)) = (journal, assignment.as_ref()) {
            record(
                journal,
                &[Entry::Leased {
                    unit: a.unit,
                    worker: worker.clone(),
                }],
            );
        }
        assignment
    };

    match assignment {
        // 204 rather than an empty object: "there is nothing to do" is not an error and not a
        // result, and a polling worker should be able to tell all three apart by status alone.
        None => respond(request, 204, "text/plain", Vec::new()),
        Some(a) => respond(
            request,
            200,
            "application/json",
            format!(
                r#"{{"unit":{},"workload":"{}","input":"{}"}}"#,
                a.unit,
                a.workload,
                hex(&a.input)
            )
            .into_bytes(),
        ),
    }
}

fn result(
    grid: &Arc<Mutex<Grid>>,
    mut request: Request,
    url: &str,
    journal: Option<&Arc<Mutex<Journal>>>,
) -> Result<(), String> {
    let (Some(unit), Some(worker)) = (query(url, "unit"), query(url, "worker")) else {
        return respond(
            request,
            400,
            "text/plain",
            b"unit= and worker= are required".to_vec(),
        );
    };
    let Ok(unit) = unit.parse::<usize>() else {
        return respond(
            request,
            400,
            "text/plain",
            b"unit= must be a number".to_vec(),
        );
    };
    let fuel = query(url, "fuel").and_then(|f| f.parse::<u64>().ok());
    // Absent means no, which is the safe default: a volunteer that cannot argue is settled for
    // by re-execution, and a volunteer wrongly assumed able to argue would be timed out and
    // convicted for silence. See `Submission::bisects`.
    let bisects = query(url, "bisects").is_some_and(|v| v == "1" || v == "true");

    // The body is the raw output bytes, hex-encoded so it survives a query-string-shaped
    // world. A workload's output is arbitrary bytes and must not be assumed to be text.
    let mut body = String::new();
    request
        .as_reader()
        .read_to_string(&mut body)
        .map_err(|e| format!("could not read body: {e}"))?;
    let Some(output) = unhex(body.trim()) else {
        return respond(request, 400, "text/plain", b"body must be hex".to_vec());
    };

    let submission = Submission {
        worker,
        output,
        fuel,
        bisects,
    };

    let outcome = {
        let mut grid = grid.lock().map_err(|_| "grid lock poisoned")?;
        let outcome = grid.submit_result(unit, submission.clone());

        // Written only when the grid *accepted* the result, and while its lock is still held.
        // Journalling a refused submission would put a result into the record that no restarted
        // coordinator would ever have taken, and journalling outside the lock would let two
        // results interleave into an order the grid never saw.
        if let (Some(journal), Ok(decided)) = (journal, &outcome) {
            record(journal, &entries_for(&grid, unit, &submission, decided));
        }

        // Rendered while the lock is held, because describing a `Disputed` outcome means
        // reading the dispute it points at.
        outcome.map(|outcome| describe(&grid, &outcome))
    };

    match outcome {
        Ok(text) => respond(request, 200, "application/json", text.into_bytes()),
        Err(refusal) => respond(request, 409, "text/plain", refusal.to_string().into_bytes()),
    }
}

/// The module a volunteer runs, or — with `?form=dispute` — the one the parties replay.
///
/// Two different programs, and the distinction is load-bearing rather than a convenience. The
/// honest-path binary carries determinism instrumentation and a fuel counter; the dispute-path
/// binary carries everything, and therefore has different instruction counts. "Step 40,000"
/// names a state only if both parties are replaying the same bytes.
fn module(grid: &Arc<Mutex<Grid>>, request: Request, path: &str, url: &str) -> Result<(), String> {
    let id = path.trim_start_matches("/api/module/");
    let disputable = query(url, "form").is_some_and(|f| f == "dispute");
    let bytes = {
        let grid = grid.lock().map_err(|_| "grid lock poisoned")?;
        grid.workload(id).map(|w| {
            if disputable {
                w.disputable.as_ref().clone()
            } else {
                w.module.clone()
            }
        })
    };
    match bytes {
        Some(bytes) => respond(request, 200, "application/wasm", bytes),
        None => respond(request, 404, "text/plain", b"no such workload".to_vec()),
    }
}

/// What a party to a dispute is being asked right now, if anything.
///
/// A volunteer polls this the way it polls `/api/lease`. There are **three** answers, and
/// collapsing the last two is what makes a party lose a dispute it was winning:
///
/// - `200` with a question — replay and answer it.
/// - `200 {"waiting":true}` — you are in a dispute and it is not your turn. The referee asks one
///   party at a time, so this is most of a party's life during a dispute.
/// - `204` — you are not in a dispute at all.
///
/// **"Not your turn" is not idleness.** A worker that treats it as such and goes home has
/// abandoned a dispute, and abandoning means losing by default — so a volunteer would be
/// convicted for *the other party* being slow. That is the class of failure this project exists
/// to avoid, and it is only avoidable if the coordinator says which of the two it means.
///
/// The reply carries the module and the input, so answering needs no memory of the unit: a
/// volunteer's entire state is its name.
fn challenge(grid: &Arc<Mutex<Grid>>, request: Request, url: &str) -> Result<(), String> {
    let Some(worker) = query(url, "worker") else {
        return respond(request, 400, "text/plain", b"worker= is required".to_vec());
    };

    let standing = {
        let grid = grid.lock().map_err(|_| "grid lock poisoned")?;
        grid.dispute_for(&worker).map(|(index, dispute)| {
            let asked = dispute.desk_for(&worker).and_then(|desk| {
                let (token, question) = desk.pending()?;
                let input = grid.unit(dispute.unit).map(|u| hex(&u.input))?;
                Some(format!(
                    r#"{{"dispute":{index},"unit":{},"token":{token},"ask":"{}","step":{},"workload":"{}","input":"{input}"}}"#,
                    dispute.unit,
                    question.kind(),
                    question.step().unwrap_or(0),
                    dispute.workload,
                ))
            });
            asked.unwrap_or_else(|| format!(r#"{{"dispute":{index},"waiting":true}}"#))
        })
    };

    match standing {
        Some(body) => respond(request, 200, "application/json", body.into_bytes()),
        None => respond(request, 204, "text/plain", Vec::new()),
    }
}

/// A party's answer to the question it was handed.
///
/// The body is hex in every case: a root, an encoded witness, or a decimal length. The `token`
/// must be the one the question came with — an answer quoting a stale token is refused, so a
/// slow party cannot have its reply to an abandoned question counted as a reply to the next.
fn answer_challenge(
    grid: &Arc<Mutex<Grid>>,
    mut request: Request,
    url: &str,
) -> Result<(), String> {
    let (Some(worker), Some(token)) = (query(url, "worker"), query(url, "token")) else {
        return respond(
            request,
            400,
            "text/plain",
            b"worker= and token= are required".to_vec(),
        );
    };
    let Ok(token) = token.parse::<u64>() else {
        return respond(
            request,
            400,
            "text/plain",
            b"token= must be a number".to_vec(),
        );
    };

    let mut body = String::new();
    request
        .as_reader()
        .read_to_string(&mut body)
        .map_err(|e| format!("could not read body: {e}"))?;
    let body = body.trim().to_owned();

    // The desk is taken out from under the grid lock: delivering an answer wakes the referee
    // thread, and holding the coordinator's lock while another thread starts work on it is how
    // a server acquires an intermittent deadlock.
    let desk = {
        let grid = grid.lock().map_err(|_| "grid lock poisoned")?;
        grid.dispute_for(&worker)
            .and_then(|(_, d)| d.desk_for(&worker).cloned())
    };
    let Some(desk) = desk else {
        return respond(
            request,
            409,
            "text/plain",
            b"you are not in a dispute".to_vec(),
        );
    };

    let Some((_, question)) = desk.pending() else {
        return respond(request, 409, "text/plain", b"nothing was asked".to_vec());
    };

    let answer = match question {
        Question::Length => match body.parse::<u64>() {
            Ok(n) => Answer::Length(n),
            Err(_) => {
                return respond(
                    request,
                    400,
                    "text/plain",
                    b"length must be a number".to_vec(),
                )
            }
        },
        // An empty body is the honest answer "my execution had ended by then", and it has to be
        // distinguishable from a missing one. It is not an error and it is not a hash.
        Question::Root { .. } if body.is_empty() => Answer::Root(None),
        Question::Root { .. } => match unhex(&body).and_then(|b| <[u8; 32]>::try_from(b).ok()) {
            Some(root) => Answer::Root(Some(root)),
            None => {
                return respond(
                    request,
                    400,
                    "text/plain",
                    b"a root is 32 hex bytes".to_vec(),
                )
            }
        },
        Question::Witness { .. } => match unhex(&body) {
            Some(bytes) => Answer::Witness(bytes),
            None => return respond(request, 400, "text/plain", b"body must be hex".to_vec()),
        },
    };

    if desk.reply(token, answer) {
        respond(
            request,
            200,
            "application/json",
            br#"{"accepted":true}"#.to_vec(),
        )
    } else {
        // Not an error on the party's side: the referee gave up, or moved on, between the
        // question being collected and the answer arriving.
        respond(
            request,
            409,
            "text/plain",
            b"that question is no longer outstanding".to_vec(),
        )
    }
}

/// Every dispute, with the questions and answers that settled it.
///
/// The transcript is the point. This project's central claim is that a disagreement about an
/// execution of any length costs a few dozen messages, and this is where somebody can count
/// them instead of taking the claim on trust.
fn disputes(grid: &Arc<Mutex<Grid>>, request: Request) -> Result<(), String> {
    let body = {
        let grid = grid.lock().map_err(|_| "grid lock poisoned")?;
        let rendered: Vec<String> = grid
            .disputes()
            .iter()
            .enumerate()
            .map(|(index, dispute)| render_dispute(index, dispute))
            .collect();
        format!("[{}]", rendered.join(","))
    };
    respond(request, 200, "application/json", body.into_bytes())
}

fn render_dispute(index: usize, dispute: &Dispute) -> String {
    let log = dispute.log();
    let transcript: Vec<String> = log
        .transcript
        .iter()
        .map(|u| {
            format!(
                r#"{{"party":"{}","step":{},"root":"{}"}}"#,
                u.party,
                u.step,
                u.root.as_ref().map(|r| hex(r)).unwrap_or_default()
            )
        })
        .collect();

    let conclusion = log.conclusion.as_ref().map_or_else(
        || r#""arguing""#.to_owned(),
        |c| format!(r#""{}""#, escape(&conclusion_words(c))),
    );

    format!(
        r#"{{"dispute":{index},"unit":{},"parties":["{}","{}"],"messages":{},"conclusion":{conclusion},{},"transcript":[{}]}}"#,
        dispute.unit,
        escape(dispute.parties.first().map_or("", String::as_str)),
        escape(dispute.parties.get(1).map_or("", String::as_str)),
        log.transcript.len(),
        verdict_fields(log.conclusion.as_ref()),
        transcript.join(",")
    )
}

/// The verdict as machine-readable fields, alongside the English prose.
///
/// # Why both
///
/// `conclusion` is a sentence, and a sentence is the wrong thing for a program to branch on —
/// anything reading it has to match English substrings, which breaks the first time the wording
/// improves. These fields say the same thing in a form a caller can act on, including one that
/// renders the verdict in another language.
///
/// `executed` is the field worth having: it is what this whole project is about, and it says how
/// much the **coordinator** ran to reach the verdict, which is `nothing` or `one-instruction`
/// except on the re-execution route.
fn verdict_fields(conclusion: Option<&Conclusion>) -> String {
    let (kind, liar, divergence, executed) = match conclusion {
        None => ("arguing", "null".to_owned(), "null".to_owned(), "nothing"),
        Some(Conclusion::Convicted {
            liar, divergence, ..
        }) => (
            "convicted",
            party_index(*liar),
            divergence.to_string(),
            "one-instruction",
        ),
        Some(Conclusion::Abandoned { by, .. }) => {
            ("abandoned", party_index(*by), "null".to_owned(), "nothing")
        }
        Some(Conclusion::BothWrong { divergence, .. }) => (
            "both-wrong",
            "null".to_owned(),
            divergence.to_string(),
            "one-instruction",
        ),
        Some(Conclusion::AgreedOnTrace { wrong, .. }) => (
            "agreed-on-trace",
            wrong.map_or_else(|| "null".to_owned(), party_index),
            "null".to_owned(),
            "nothing",
        ),
        Some(Conclusion::FellBack { .. }) => (
            "fell-back",
            "null".to_owned(),
            "null".to_owned(),
            "the-whole-unit",
        ),
    };

    format!(
        r#""kind":"{kind}","atFault":{liar},"divergence":{divergence},"rounds":{},"executed":"{executed}""#,
        conclusion.map_or(0, Conclusion::rounds)
    )
}

/// A party as an index into `parties`, so a caller need not parse "first party".
fn party_index(party: Party) -> String {
    match party {
        Party::First => "0".to_owned(),
        Party::Second => "1".to_owned(),
    }
}

fn conclusion_words(conclusion: &Conclusion) -> String {
    match conclusion {
        Conclusion::Convicted {
            liar,
            divergence,
            rounds,
        } => format!(
            "the {liar} lied about the instruction at step {divergence}, \
             found in {rounds} rounds of bisection and proved by executing that one instruction"
        ),
        Conclusion::Abandoned { by, rounds } => {
            format!("the {by} stopped answering after {rounds} rounds and loses by default")
        }
        Conclusion::AgreedOnTrace {
            wrong: Some(wrong),
            rounds,
        } => format!(
            "nobody lied — both parties' replays agreed, and the trace they agreed on says the \
             {wrong} reported the wrong answer ({rounds} messages, nothing executed)"
        ),
        Conclusion::AgreedOnTrace {
            wrong: None,
            rounds,
        } => format!(
            "both parties agreed on a trace and both misreported what it answered \
             ({rounds} messages, nothing executed)"
        ),
        Conclusion::BothWrong { divergence, rounds } => format!(
            "both parties were wrong about the instruction at step {divergence} ({rounds} rounds)"
        ),
        Conclusion::FellBack { why, verdict } => {
            format!("bisection could not settle it — {why} — so {verdict}")
        }
    }
}

fn status(grid: &Arc<Mutex<Grid>>, request: Request) -> Result<(), String> {
    let body = {
        let grid = grid.lock().map_err(|_| "grid lock poisoned")?;
        let units: Vec<String> = grid
            .units()
            .iter()
            .enumerate()
            .map(|(i, unit)| {
                format!(
                    r#"{{"unit":{},"quorum":{},"results":{},"outcome":{}}}"#,
                    i,
                    unit.quorum,
                    unit.results.len(),
                    describe(&grid, &unit.outcome)
                )
            })
            .collect();
        format!("[{}]", units.join(","))
    };
    respond(request, 200, "application/json", body.into_bytes())
}

/// Serve the browser worker, so the whole system is one command.
fn static_file(request: Request, root: &str, path: &str) -> Result<(), String> {
    let relative = if path == "/" { "/index.html" } else { path };

    // Resolve, then check containment. Checking the raw path for `..` is the version that gets
    // bypassed; checking where it landed is the version that does not.
    let root_path = std::path::Path::new(root)
        .canonicalize()
        .map_err(|e| format!("web root {root}: {e}"))?;
    let target = root_path.join(relative.trim_start_matches('/'));
    let Ok(target) = target.canonicalize() else {
        return respond(request, 404, "text/plain", b"not found".to_vec());
    };
    if !target.starts_with(&root_path) {
        return respond(request, 403, "text/plain", b"outside the web root".to_vec());
    }

    let content_type = match target.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        // Matters: `WebAssembly.instantiateStreaming` refuses anything else, and the failure
        // message does not mention the content type.
        Some("wasm") => "application/wasm",
        Some("md") => "text/markdown; charset=utf-8",
        _ => "application/octet-stream",
    };

    match std::fs::read(&target) {
        Ok(bytes) => respond(request, 200, content_type, bytes),
        Err(_) => respond(request, 404, "text/plain", b"not found".to_vec()),
    }
}

// --- plumbing --------------------------------------------------------------------------------

/// One unit's outcome as JSON.
///
/// Takes the grid because a `Disputed` outcome is a *pointer*: the argument is a live process
/// with a growing transcript, so the answer has to be read out of it rather than copied into
/// the unit when the dispute opened.
fn describe(grid: &Grid, outcome: &Outcome) -> String {
    match outcome {
        Outcome::Open => r#"{"state":"open"}"#.to_owned(),
        Outcome::Accepted { output } => {
            format!(r#"{{"state":"accepted","output":"{}"}}"#, hex(output))
        }
        Outcome::Settled { verdict, output } => format!(
            r#"{{"state":"settled","by":"re-execution","verdict":"{}","output":"{}"}}"#,
            escape(verdict),
            output.as_ref().map(|o| hex(o)).unwrap_or_default()
        ),
        Outcome::Disputed { dispute } => {
            let Some(argument) = grid.dispute(*dispute) else {
                return r#"{"state":"disputed"}"#.to_owned();
            };
            let log = argument.log();
            let Some(conclusion) = log.conclusion.as_ref() else {
                return format!(
                    r#"{{"state":"arguing","dispute":{dispute},"messages":{}}}"#,
                    log.transcript.len()
                );
            };
            format!(
                r#"{{"state":"settled","by":"bisection","dispute":{dispute},"rounds":{},"messages":{},"verdict":"{}","output":"{}"}}"#,
                conclusion.rounds(),
                log.transcript.len(),
                escape(&conclusion_words(conclusion)),
                log.output.as_ref().map(|o| hex(o)).unwrap_or_default()
            )
        }
    }
}

fn respond(request: Request, code: u16, content_type: &str, body: Vec<u8>) -> Result<(), String> {
    // A worker names itself, and a name is a stranger's bytes: it reaches `/api/disputes` and
    // `/api/status`, and it is routinely not ASCII. JSON is UTF-8 by specification, but a client
    // told only `application/json` is entitled to guess — and some do, producing mojibake that
    // looks like a bug in the coordinator.
    let content_type = if content_type == "application/json" {
        "application/json; charset=utf-8"
    } else {
        content_type
    };
    let header = Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes())
        .map_err(|()| "bad content type".to_owned())?;
    // The browser worker is served from the same origin, so CORS is not needed for it — but a
    // volunteer running the page from somewhere else is exactly the deployment this project is
    // for, and refusing them would be an odd way to run a volunteer network.
    let cors = Header::from_bytes(&b"Access-Control-Allow-Origin"[..], &b"*"[..])
        .map_err(|()| "bad header".to_owned())?;

    request
        .respond(
            Response::from_data(body)
                .with_status_code(code)
                .with_header(header)
                .with_header(cors),
        )
        .map_err(|e| format!("could not respond: {e}"))
}

/// One query parameter, percent-decoding nothing: every value this API takes is hex, a number,
/// or a worker name, and a worker name with a `%` in it is a worker name that can be renamed.
fn query(url: &str, key: &str) -> Option<String> {
    url.split('?')
        .nth(1)?
        .split('&')
        .find_map(|pair| pair.strip_prefix(key)?.strip_prefix('=').map(str::to_owned))
        .filter(|value| !value.is_empty())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, byte| {
        out.push_str(&format!("{byte:02x}"));
        out
    })
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

/// Escape for a JSON string literal. Only what can appear in a verdict, which is text this
/// crate writes — but a workload's name reaches the status endpoint too, and that is a
/// stranger's bytes.
fn escape(text: &str) -> String {
    text.chars()
        .flat_map(|c| match c {
            '"' => vec!['\\', '"'],
            '\\' => vec!['\\', '\\'],
            '\n' => vec!['\\', 'n'],
            c if (c as u32) < 0x20 => vec![' '],
            c => vec![c],
        })
        .collect()
}
