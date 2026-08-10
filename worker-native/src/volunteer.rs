//! A volunteer that can argue: takes units, and answers challenges about them.
//!
//! The browser worker in `browser/` is the volunteer this project is *for* — no install, full
//! speed, and blind. This one is the other kind. It runs the same work on the same compiler, and
//! when somebody disputes a result it can also do the thing a browser never will: replay the
//! unit under Cairn's interpreter and say what state it was in after *n* instructions.
//!
//! That is the only reason this mode exists. A network of blind volunteers has to settle every
//! disagreement by re-execution; one with a few arguing volunteers settles them in `log₂(n)`
//! messages and a single executed instruction. See
//! [ADR-0011](../../docs/adr/0011-a-volunteer-that-cannot-argue-is-not-challenged.md).
//!
//! # The whole client
//!
//! ```text
//! GET  /api/challenge?worker=NAME     am I being asked something?
//! POST /api/challenge?worker=NAME&token=T     the answer
//! GET  /api/lease?worker=NAME         anything to do?
//! GET  /api/module/{id}               the bytes to run
//! GET  /api/module/{id}?form=dispute  the bytes to replay
//! POST /api/result?unit=N&worker=NAME&fuel=F&bisects=1
//! ```
//!
//! Challenges are polled **before** work, because a party to a running dispute is holding one up
//! and a unit can wait. No state is kept across polls beyond a module cache and a name.
//!
//! # The one rule that is easy to get wrong
//!
//! The referee asks one party at a time, so a party spends most of a dispute with **nothing
//! outstanding** — and that is not the same as having no dispute. A worker that counts it as
//! idleness and goes home has *abandoned* a dispute, which means losing by default: convicted
//! because the other party was slow. `/api/challenge` therefore answers with three states, and
//! [`Turn`] is the client half of that.

use std::collections::HashMap;
use std::thread;
use std::time::{Duration, Instant};

use cairn_runtime::dispute::{self, Answer, Claimant as _, Question, Replay, Step};
use cairn_runtime::engine::image::{self, Image};
use cairn_runtime::engine::machine::Limits;

use crate::client;
use crate::host;

/// How long to wait after finding nothing to do.
///
/// Short enough that a demo does not feel dead, long enough not to be a busy loop against
/// somebody else's server.
const IDLE: Duration = Duration::from_millis(500);

/// How long to wait for the other party's turn to end.
///
/// Shorter than [`IDLE`], because a dispute is `2 log2(n)` questions asked strictly one after
/// another: every poll interval on this path is paid twenty times over on a million-instruction
/// unit, and it dominates the wall-clock cost of a dispute far more than the replays do.
const TAKING_TURNS: Duration = Duration::from_millis(60);

/// A volunteer's settings.
pub struct Volunteer<'a> {
    /// Coordinator origin, e.g. `http://127.0.0.1:8080`.
    pub base: &'a str,
    /// This volunteer's name. One name, one vote.
    pub name: &'a str,
    /// Corrupt everything from this step onwards, to demonstrate what happens then.
    ///
    /// **A liar has to lie twice.** The wrong answer is what starts a dispute; lying in the
    /// replay is what makes the dispute convictable. A party that returns a wrong answer and
    /// then replays honestly agrees with everybody and is convicted of nothing — its replay
    /// reproduces the truth, because the replay is deterministic. So this corrupts the output
    /// *and* every root at or after the given step.
    pub lies_from: Option<u64>,
    /// Stop after this many consecutive idle polls. `None` runs until killed.
    pub idle_exit: Option<u32>,
}

/// Poll a coordinator, do its work, and answer for it afterwards.
///
/// # Errors
///
/// Anything that stops the volunteer talking to the coordinator at all. A single failed unit is
/// reported and skipped: a volunteer that exits because one request failed is a volunteer that
/// leaves the network over a dropped packet.
pub fn serve(settings: &Volunteer<'_>) -> Result<(), String> {
    println!("volunteer     {}", settings.name);
    println!("coordinator   {}", settings.base);
    println!(
        "can argue     yes — replays under Cairn's interpreter, so it can be a party to a dispute"
    );
    if let Some(from) = settings.lies_from {
        println!("DISHONEST     corrupting results, and every claimed root from step {from}");
    }
    println!();

    let mut kept = Kept::default();
    let mut idle = 0;
    let mut answered: Option<u64> = None;

    loop {
        // A dispute is holding two volunteers and a unit; work can wait.
        match challenge(settings, &mut kept, &mut answered) {
            Ok(Turn::Answered) => {
                idle = 0;
                continue;
            }
            // In a dispute, and it is the other party's turn. NOT idle: leaving now would be
            // abandoning a dispute, and a party that abandons loses by default.
            Ok(Turn::Waiting) => {
                idle = 0;
                thread::sleep(TAKING_TURNS);
                continue;
            }
            Ok(Turn::Nothing) => {}
            Err(e) => eprintln!("challenge failed: {e}"),
        }

        match take_a_unit(settings, &mut kept) {
            Ok(true) => {
                idle = 0;
                continue;
            }
            Ok(false) => {}
            Err(e) => eprintln!("unit failed: {e}"),
        }

        idle += 1;
        if settings.idle_exit.is_some_and(|limit| idle >= limit) {
            println!("nothing left to do");
            return Ok(());
        }
        thread::sleep(IDLE);
    }
}

/// What a poll of `/api/challenge` found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Turn {
    /// A question was asked and has been answered.
    Answered,
    /// In a dispute, nothing outstanding. The referee is asking the other party.
    Waiting,
    /// Not a party to any running dispute.
    Nothing,
}

/// What a volunteer remembers between polls.
///
/// # Why a replay is kept per dispute
///
/// A bisection asks about `log2(n)` questions. Answering each from a fresh replay costs `O(n)`,
/// so a dispute costs `O(n log n)` — on top of the interpreter being 37x-142x slower than the
/// engine the work was done on. [`Replay`] exists to fix exactly that: it keeps periodic machine
/// states and resumes from the nearest one, which makes a whole dispute `O(n)`. Discarding it
/// after each question would leave the mechanism in place and pay none of it.
///
/// Reusing one across questions is safe, and there is a test for it in the runtime —
/// `checkpointing_changes_speed_and_nothing_else` — because a resumed machine is a bit-exact
/// copy of one that got there by stepping. A warm replay and a cold one answer identically, so
/// this is a speed decision and never a protocol one.
#[derive(Default)]
struct Kept {
    /// Honest-path bytes, for wasmtime.
    running: HashMap<String, Vec<u8>>,
    /// Dispute-path images, decoded once.
    ///
    /// Leaked deliberately. A [`Replay`] borrows its image and a volunteer holds one for as long
    /// as a dispute lasts; making that borrow live long enough without leaking means a
    /// self-referential struct, or an `Arc` the runtime does not ask for. A volunteer sees a
    /// handful of workloads of a few kilobytes each, and this cache never evicted them anyway.
    images: HashMap<String, &'static Image<'static>>,
    /// One replay per dispute, so its checkpoints survive between questions.
    replays: HashMap<u64, Replay<'static>>,
}

/// Answer a challenge if there is one. `true` if something was done.
fn challenge(
    settings: &Volunteer<'_>,
    kept: &mut Kept,
    answered: &mut Option<u64>,
) -> Result<Turn, String> {
    let asked = client::get(&format!(
        "{}/api/challenge?worker={}",
        settings.base, settings.name
    ))?;
    if asked.status == 204 {
        return Ok(Turn::Nothing);
    }
    if asked.status != 200 {
        return Err(format!("challenge poll returned {}", asked.status));
    }

    let body = asked.text();
    if client::field(&body, "waiting").is_some() {
        return Ok(Turn::Waiting);
    }
    let read = |key: &str| {
        client::field(&body, key).ok_or_else(|| format!("no {key} in challenge {body:?}"))
    };

    let token: u64 = read("token")?
        .parse()
        .map_err(|_| "token was not a number".to_owned())?;
    // A party polls faster than the referee advances, so the same question shows up more than
    // once. Answering it again is harmless but costs an interpreted replay, which is the
    // expensive thing here.
    if *answered == Some(token) {
        // Still in the dispute — the referee has simply not collected this answer yet.
        return Ok(Turn::Waiting);
    }

    let step: u64 = read("step")?
        .parse()
        .map_err(|_| "step was not a number".to_owned())?;
    let question = match read("ask")?.as_str() {
        "length" => Question::Length,
        "root" => Question::Root { step },
        "witness" => Question::Witness { step },
        other => return Err(format!("unknown question {other:?}")),
    };

    let dispute_id: u64 = read("dispute")?
        .parse()
        .map_err(|_| "dispute was not a number".to_owned())?;
    let workload = read("workload")?;
    let input = unhex(&read("input")?).ok_or_else(|| "input was not hex".to_owned())?;

    // The *dispute-path* module: a different program from the one this volunteer ran, with
    // different instruction counts. "Step 40,000" names a state only if both parties replay the
    // same bytes.
    let image = dispute_image(settings, kept, &workload)?;

    let started = Instant::now();
    let honest = match question {
        // The one question asked repeatedly, and the only one worth keeping a replay warm for.
        // Everything else happens once per dispute.
        Question::Root { step } => {
            let replay = kept
                .replays
                .entry(dispute_id)
                .or_insert_with(|| Replay::new(image, input.clone(), Limits::default()));
            Answer::Root(replay.root_at(Step::new(step)).unwrap_or(None))
        }
        other => dispute::answer(image, &input, Limits::default(), other)
            .map_err(|e| format!("could not answer: {e}"))?,
    };
    let answer = distort(settings, question, honest);
    let elapsed = started.elapsed();

    let body = match &answer {
        Answer::Length(n) => n.to_string(),
        Answer::Root(Some(root)) => hex(root),
        // An empty body is the answer "my execution had ended by then", and the coordinator has
        // to be able to tell it from a missing one.
        Answer::Root(None) => String::new(),
        Answer::Witness(bytes) => hex(bytes),
    };

    let posted = client::post(
        &format!(
            "{}/api/challenge?worker={}&token={token}",
            settings.base, settings.name
        ),
        body.as_bytes(),
    )?;

    println!(
        "challenge     {} at step {step} — answered in {elapsed:.1?} ({})",
        question.kind(),
        if posted.status == 200 {
            "accepted"
        } else {
            "no longer wanted"
        }
    );
    *answered = Some(token);
    Ok(Turn::Answered)
}

/// Take one unit, run it, report the answer. `true` if there was one.
fn take_a_unit(settings: &Volunteer<'_>, kept: &mut Kept) -> Result<bool, String> {
    let leased = client::get(&format!(
        "{}/api/lease?worker={}",
        settings.base, settings.name
    ))?;
    if leased.status == 204 {
        return Ok(false);
    }
    if leased.status != 200 {
        return Err(format!("lease returned {}", leased.status));
    }

    let body = leased.text();
    let read = |key: &str| client::field(&body, key).ok_or_else(|| format!("no {key} in {body:?}"));

    let unit = read("unit")?;
    let workload = read("workload")?;
    let input = unhex(&read("input")?).ok_or_else(|| "input was not hex".to_owned())?;

    let module = running_module(settings, kept, &workload)?;
    let started = Instant::now();
    let executed = host::execute(module, &input)?;
    let elapsed = started.elapsed();

    // A liar's answer has to be wrong, or there is nothing to dispute.
    let output = if settings.lies_from.is_some() {
        vec![0xde, 0xad, 0xbe, 0xef]
    } else {
        executed.output
    };

    let mut url = format!(
        "{}/api/result?unit={unit}&worker={}&bisects=1",
        settings.base, settings.name
    );
    if let Some(fuel) = executed.fuel {
        url.push_str(&format!("&fuel={fuel}"));
    }

    let reported = client::post(&url, hex(&output).as_bytes())?;
    println!(
        "unit {unit:<3}      {elapsed:.1?}, {} → {}",
        executed.fuel.map_or_else(
            || "cost not reported".to_owned(),
            |f| format!("{f} instructions")
        ),
        reported.text()
    );
    Ok(true)
}

/// Corrupt an answer, if this volunteer was told to be dishonest.
fn distort(settings: &Volunteer<'_>, question: Question, honest: Answer) -> Answer {
    let (Some(from), Question::Root { step }, Answer::Root(root)) =
        (settings.lies_from, question, &honest)
    else {
        return honest;
    };
    if step < from {
        return honest;
    }
    Answer::Root(root.map(|mut r| {
        // One flipped byte. A liar has no reason to be creative: any root but the true one is a
        // claim the disputed instruction will refute.
        if let Some(first) = r.first_mut() {
            *first ^= 0xff;
        }
        r
    }))
}

/// The honest-path bytes for a workload, fetched once and kept.
fn running_module<'a>(
    settings: &Volunteer<'_>,
    kept: &'a mut Kept,
    workload: &str,
) -> Result<&'a [u8], String> {
    if !kept.running.contains_key(workload) {
        let bytes = download(settings, workload, false)?;
        kept.running.insert(workload.to_owned(), bytes);
    }
    kept.running
        .get(workload)
        .map(Vec::as_slice)
        .ok_or_else(|| "the module vanished from the cache".to_owned())
}

/// The dispute-path image for a workload, decoded once and kept for as long as this runs.
fn dispute_image(
    settings: &Volunteer<'_>,
    kept: &mut Kept,
    workload: &str,
) -> Result<&'static Image<'static>, String> {
    if let Some(image) = kept.images.get(workload) {
        return Ok(image);
    }
    let bytes: &'static [u8] = Box::leak(download(settings, workload, true)?.into_boxed_slice());
    let image: &'static Image<'static> = Box::leak(Box::new(
        image::decode(bytes).map_err(|e| format!("could not decode the module: {e}"))?,
    ));
    kept.images.insert(workload.to_owned(), image);
    Ok(image)
}

fn download(settings: &Volunteer<'_>, workload: &str, disputable: bool) -> Result<Vec<u8>, String> {
    let suffix = if disputable { "?form=dispute" } else { "" };
    let fetched = client::get(&format!("{}/api/module/{workload}{suffix}", settings.base))?;
    if fetched.status != 200 {
        return Err(format!("module {workload}: {}", fetched.status));
    }
    Ok(fetched.body)
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
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
