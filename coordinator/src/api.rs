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
//! POST /api/result?unit=N&worker=NAME&fuel=F     return an answer, body = raw output bytes
//! GET  /api/module/{id}           the canonical module a volunteer executes
//! GET  /api/status                every unit and where it got to
//! GET  /                          the browser worker, served from browser/
//! ```
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

use crate::grid::{Grid, Outcome, Submission};

/// Serve until killed.
///
/// # Errors
///
/// If the address cannot be bound.
pub fn serve(grid: Arc<Mutex<Grid>>, address: &str, web_root: Option<&str>) -> Result<(), String> {
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
        if let Err(e) = handle(&grid, request, web_root) {
            eprintln!("request failed: {e}");
        }
    }
    Ok(())
}

fn handle(grid: &Arc<Mutex<Grid>>, request: Request, web_root: Option<&str>) -> Result<(), String> {
    let url = request.url().to_owned();
    let path = url.split('?').next().unwrap_or("").to_owned();

    match path.as_str() {
        "/api/lease" => lease(grid, request, &url),
        "/api/result" => result(grid, request, &url),
        "/api/status" => status(grid, request),
        p if p.starts_with("/api/module/") => module(grid, request, p),
        _ => match web_root {
            Some(root) => static_file(request, root, &path),
            None => respond(request, 404, "text/plain", b"not found".to_vec()),
        },
    }
}

// --- endpoints -------------------------------------------------------------------------------

fn lease(grid: &Arc<Mutex<Grid>>, request: Request, url: &str) -> Result<(), String> {
    let Some(worker) = query(url, "worker") else {
        return respond(request, 400, "text/plain", b"worker= is required".to_vec());
    };

    let assignment = {
        let mut grid = grid.lock().map_err(|_| "grid lock poisoned")?;
        grid.lease(&worker, Instant::now())
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

fn result(grid: &Arc<Mutex<Grid>>, mut request: Request, url: &str) -> Result<(), String> {
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

    let outcome = {
        let mut grid = grid.lock().map_err(|_| "grid lock poisoned")?;
        grid.submit_result(
            unit,
            Submission {
                worker,
                output,
                fuel,
            },
        )
        .map(|outcome| describe(&outcome))
    };

    match outcome {
        Ok(text) => respond(request, 200, "application/json", text.into_bytes()),
        Err(refusal) => respond(request, 409, "text/plain", refusal.to_string().into_bytes()),
    }
}

fn module(grid: &Arc<Mutex<Grid>>, request: Request, path: &str) -> Result<(), String> {
    let id = path.trim_start_matches("/api/module/");
    let bytes = {
        let grid = grid.lock().map_err(|_| "grid lock poisoned")?;
        grid.workload(id).map(|w| w.module.clone())
    };
    match bytes {
        Some(bytes) => respond(request, 200, "application/wasm", bytes),
        None => respond(request, 404, "text/plain", b"no such workload".to_vec()),
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
                    describe(&unit.outcome)
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

fn describe(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Open => r#"{"state":"open"}"#.to_owned(),
        Outcome::Accepted { output } => {
            format!(r#"{{"state":"accepted","output":"{}"}}"#, hex(output))
        }
        Outcome::Settled {
            divergence,
            rounds,
            verdict,
            output,
        } => format!(
            r#"{{"state":"settled","divergence":{divergence},"rounds":{rounds},"verdict":"{}","output":"{}"}}"#,
            escape(verdict),
            output.as_ref().map(|o| hex(o)).unwrap_or_default()
        ),
    }
}

fn respond(request: Request, code: u16, content_type: &str, body: Vec<u8>) -> Result<(), String> {
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
