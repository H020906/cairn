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
//! No state is kept across polls beyond a module cache and a name.
//!
//! # The shape: many workers, one arguer, one name
//!
//! A donated machine has more than one core, and units share nothing — each gets its own
//! instance and its own linear memory, and runs single-threaded — so several run side by side.
//! [`capacity`] decides how many, and does so from memory rather than from cores.
//!
//! Two things about that shape are load-bearing, and neither is obvious:
//!
//! **All of those threads report under one worker name.** A sixteen-core machine that registered
//! as sixteen volunteers could satisfy both halves of a replicated unit *by itself*, and the
//! quorum would be two executions on one machine by one operator — which is not replication at
//! all, it is a machine agreeing with itself. The coordinator enforces one vote per name
//! (`Grid::lease` skips units this worker has already answered), so sharing the name is what
//! keeps that guarantee true. One machine, one name, one vote.
//!
//! **Arguing stays on a single thread.** Not for simplicity: a party to a dispute holds up to
//! `DEFAULT_CHECKPOINT_BUDGET` clones of the machine, which is tens of times what executing the
//! same unit costs, and it is the *unbudgeted* memory in a volunteer. The referee asks one party
//! one question at a time anyway, so there is nothing to gain by spreading it out and a machine
//! to lose.
//!
//! # The one rule that is easy to get wrong
//!
//! The referee asks one party at a time, so a party spends most of a dispute with **nothing
//! outstanding** — and that is not the same as having no dispute. A worker that counts it as
//! idleness and goes home has *abandoned* a dispute, which means losing by default: convicted
//! because the other party was slow. `/api/challenge` therefore answers with three states, and
//! [`Turn`] is the client half of that.
//!
//! This is why the arguing thread outlives the working ones. When the last unit thread gives up
//! on an empty queue, the arguer keeps polling for `--idle-exit` more rounds and any question at
//! all resets the count. Leaving is the *last* thing a volunteer does.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use cairn_runtime::dispute::{self, Answer, Claimant as _, Question, Replay, Step};
use cairn_runtime::engine::image::{self, Image};
use cairn_runtime::engine::machine::Limits;
use cairn_runtime::validate;

use crate::capacity::{self, Allowance, Survey};
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

/// How far apart to start the unit threads.
///
/// The coordinator serves requests one at a time. Threads that start together stay together —
/// they lease together, finish together, and poll together — which turns a steady trickle of
/// requests into a burst every few hundred milliseconds. One sleep at startup is enough to break
/// the convoy.
///
/// It is not free, and the cost is measurable: the ramp is `jobs × this`, and every thread that
/// has not started yet is a core standing idle. At 25 ms and fifteen jobs it cost about 5% of a
/// seven-second run, which is why it is 10.
const STAGGER: Duration = Duration::from_millis(10);

/// A volunteer's settings.
pub struct Volunteer<'a> {
    /// Coordinator origin, e.g. `http://127.0.0.1:8080`.
    pub base: &'a str,
    /// This volunteer's name. **One name, one vote** — every thread reports under it.
    pub name: &'a str,
    /// An upper bound on units run at once, if the operator wants to donate less than the
    /// machine could give. It can only lower the number [`capacity::threads`] arrives at.
    pub jobs: Option<usize>,
    /// The machine's spare memory in bytes, if the operator stated it.
    ///
    /// Only Linux can be asked this without `unsafe`, so everywhere else the alternative is a
    /// conservative assumption printed as an assumption. See [`capacity::survey`].
    pub memory: Option<u64>,
    /// Corrupt everything from this step onwards, to demonstrate what happens then.
    ///
    /// **A liar has to lie twice.** The wrong answer is what starts a dispute; lying in the
    /// replay is what makes the dispute convictable. A party that returns a wrong answer and
    /// then replays honestly agrees with everybody and is convicted of nothing — its replay
    /// reproduces the truth, because the replay is deterministic. So this corrupts the output
    /// *and* every root at or after the given step.
    pub lies_from: Option<u64>,
    /// Return a wrong answer but replay **honestly** — a broken engine, not a liar.
    ///
    /// The distinction the protocol turns on. This party is not caught by bisection at all: its
    /// replay reproduces the truth and agrees with everybody, so nobody is convicted. It is
    /// caught by the trace the two parties agree on, which says what the answer was, because
    /// the answer is part of the committed state
    /// ([ADR-0012](../../docs/adr/0012-the-answer-is-part-of-the-committed-state.md)).
    pub wrong_answer: bool,
    /// Stop after this many consecutive idle polls. `None` runs until killed.
    pub idle_exit: Option<u32>,
}

impl Volunteer<'_> {
    /// Whether this volunteer returns a result it did not compute.
    ///
    /// A liar has to lie twice, so `--lie-from` implies this; `--wrong-answer` is the first lie
    /// without the second.
    const fn returns_a_wrong_answer(&self) -> bool {
        self.lies_from.is_some() || self.wrong_answer
    }
}

/// Poll a coordinator, do its work on every core this machine can spare, and answer for it.
///
/// # Errors
///
/// Anything that stops the volunteer talking to the coordinator at all. A single failed unit is
/// reported and skipped: a volunteer that exits because one request failed is a volunteer that
/// leaves the network over a dropped packet.
pub fn serve(settings: &Volunteer<'_>) -> Result<(), String> {
    let survey = capacity::survey(settings.memory);
    let jobs = capacity::threads(&survey, settings.jobs);
    // Half the budget for units in flight. The rest is the machine's, and a dispute borrows a
    // quarter of it — see `capacity`'s module docs for why arguing is the expensive part.
    let shared = Shared {
        allowance: Allowance::new(survey.memory.bytes() / 2),
        modules: Mutex::new(HashMap::new()),
        working: AtomicUsize::new(jobs),
        survey,
    };

    announce(settings, &shared, jobs);
    let shared = &shared;

    thread::scope(|scope| {
        for job in 0..jobs {
            scope.spawn(move || {
                thread::sleep(STAGGER * u32::try_from(job).unwrap_or(u32::MAX));
                work(settings, shared, job);
            });
        }
        // The arguing thread is this one, and it is the last to leave.
        argue(settings, shared)
    })
}

fn announce(settings: &Volunteer<'_>, shared: &Shared, jobs: usize) {
    println!("volunteer     {}", settings.name);
    println!("coordinator   {}", settings.base);
    println!(
        "cores         {} hardware threads, {jobs} donated",
        shared.survey.cores
    );
    println!(
        "memory        {} {}",
        human(shared.survey.memory.bytes()),
        shared.survey.memory.provenance()
    );
    println!(
        "              {} for units in flight; a dispute may draw {} more",
        human(shared.allowance.total()),
        human(shared.survey.memory.bytes() / 4)
    );
    println!(
        "can argue     yes — replays under Cairn's interpreter, so it can be a party to a dispute"
    );
    if let Some(from) = settings.lies_from {
        println!("DISHONEST     wrong results, and every claimed root corrupted from step {from}");
    } else if settings.wrong_answer {
        println!(
            "BROKEN        wrong results, but replays honestly — not a liar, and bisection will \
             not convict it"
        );
    }
    println!();
}

/// What the unit threads and the arguing thread share.
struct Shared {
    /// Memory permits. Concurrency is whatever this admits, which is the point of it.
    allowance: Arc<Allowance>,
    /// Honest-path bytes, downloaded once for the whole machine rather than once per thread.
    modules: Mutex<HashMap<String, Held>>,
    /// Unit threads still running. The arguing thread waits for this to reach zero.
    working: AtomicUsize,
    /// What was learned about the machine, kept for the checkpoint budget a dispute gets.
    survey: Survey,
}

/// A cached module and what it costs to run one.
#[derive(Clone)]
struct Held {
    bytes: Arc<Vec<u8>>,
    /// The declared memory ceiling in bytes, which is what a thread claims before executing.
    declares: u64,
}

/// One unit thread: lease, run, report, repeat.
fn work(settings: &Volunteer<'_>, shared: &Shared, job: usize) {
    let mut idle = 0_u32;
    loop {
        let busy = match take_a_unit(settings, shared, job) {
            Ok(busy) => busy,
            Err(e) => {
                eprintln!("[job {job:02}] unit failed: {e}");
                false
            }
        };

        if busy {
            idle = 0;
            continue;
        }

        idle = idle.saturating_add(1);
        if settings.idle_exit.is_some_and(|limit| idle >= limit) {
            break;
        }
        thread::sleep(IDLE);
    }
    // Announce departure *after* the loop, so the arguing thread cannot start its own countdown
    // while this one is still capable of picking up work.
    shared.working.fetch_sub(1, Ordering::SeqCst);
}

/// The arguing thread: answer challenges, and be the last to leave.
fn argue(settings: &Volunteer<'_>, shared: &Shared) -> Result<(), String> {
    let mut kept = Arguing::default();
    let mut answered: Option<u64> = None;
    let mut idle = 0_u32;

    loop {
        match challenge(settings, shared, &mut kept, &mut answered) {
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

        // Only once every unit thread has given up is there any question of leaving — and even
        // then it takes another `--idle-exit` rounds of silence, any one of which a question
        // would reset.
        if shared.working.load(Ordering::SeqCst) == 0 {
            idle = idle.saturating_add(1);
            if settings.idle_exit.is_some_and(|limit| idle >= limit) {
                println!("nothing left to do");
                return Ok(());
            }
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

/// What the arguing thread remembers between polls. Never touched by a unit thread.
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
///
/// How *many* states it keeps is the same kind of decision, and it is the one that has to fit on
/// the machine: see [`capacity::checkpoints`].
#[derive(Default)]
struct Arguing {
    /// Dispute-path images, decoded once, with the checkpoint budget this machine can afford
    /// for a dispute over one.
    ///
    /// Leaked deliberately. A [`Replay`] borrows its image and a volunteer holds one for as long
    /// as a dispute lasts; making that borrow live long enough without leaking means a
    /// self-referential struct, or an `Arc` the runtime does not ask for. A volunteer sees a
    /// handful of workloads of a few kilobytes each, and this cache never evicted them anyway.
    images: HashMap<String, (&'static Image<'static>, usize)>,
    /// One replay per dispute, so its checkpoints survive between questions.
    replays: HashMap<u64, Replay<'static>>,
}

/// Answer a challenge if there is one.
fn challenge(
    settings: &Volunteer<'_>,
    shared: &Shared,
    kept: &mut Arguing,
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
    let (image, budget) = dispute_image(settings, shared, kept, &workload)?;

    let started = Instant::now();
    let honest = match question {
        // The one question asked repeatedly, and the only one worth keeping a replay warm for.
        // Everything else happens once per dispute.
        Question::Root { step } => {
            let replay = kept.replays.entry(dispute_id).or_insert_with(|| {
                Replay::with_checkpoint_budget(image, input.clone(), Limits::default(), budget)
            });
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
fn take_a_unit(settings: &Volunteer<'_>, shared: &Shared, job: usize) -> Result<bool, String> {
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

    let module = running_module(settings, shared, &workload)?;

    // Pay for the memory before touching it, and give it back on the way out however this
    // returns. Between here and the drop is the only window in which this thread costs the
    // machine anything, which is why the claim is this narrow: downloading, parsing and
    // reporting all happen outside it.
    let queued = Instant::now();
    let (executed, elapsed) = {
        let _claim = shared.allowance.claim(module.declares);
        let waited = queued.elapsed();
        if waited > Duration::from_millis(1) {
            println!("[job {job:02}] waited {waited:.1?} for memory before unit {unit}");
        }
        let started = Instant::now();
        (host::execute(&module.bytes, &input)?, started.elapsed())
    };

    // A wrong answer is what starts a dispute; whether the party then lies about the trace is
    // what decides how it ends.
    let output = if settings.returns_a_wrong_answer() {
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
        "[job {job:02}] unit {unit:<3} {elapsed:.1?}, {} → {}",
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

/// The honest-path bytes for a workload, fetched once for the whole machine.
///
/// The download happens **outside** the cache lock. Holding a mutex across a network request
/// would mean the first thread to want a new workload stops every other thread from starting
/// anything at all, which is a straightforward way to turn sixteen cores back into one. Two
/// threads racing to download the same module is a wasted request and nothing worse.
fn running_module(
    settings: &Volunteer<'_>,
    shared: &Shared,
    workload: &str,
) -> Result<Held, String> {
    if let Some(held) = shared
        .modules
        .lock()
        .map_err(|_| "module cache poisoned")?
        .get(workload)
    {
        return Ok(held.clone());
    }

    let bytes = download(settings, workload, false)?;
    // A module that declares no maximum was refused at registration, so this only happens if the
    // coordinator served something it never admitted. Assume the worst the network permits
    // rather than assume nothing: under-parallelising is recoverable and over-committing is not.
    let declares = capacity::per_unit_bytes(
        validate::declared_memory_pages(&bytes)
            .unwrap_or_else(|| validate::Limits::default().max_memory_pages),
    );
    let held = Held {
        bytes: Arc::new(bytes),
        declares,
    };

    shared
        .modules
        .lock()
        .map_err(|_| "module cache poisoned")?
        .insert(workload.to_owned(), held.clone());
    Ok(held)
}

/// The dispute-path image for a workload, decoded once and kept for as long as this runs.
///
/// Returns the checkpoint budget with it, because both are properties of the workload and
/// deciding the budget twice is how the two would come to disagree.
fn dispute_image(
    settings: &Volunteer<'_>,
    shared: &Shared,
    kept: &mut Arguing,
    workload: &str,
) -> Result<(&'static Image<'static>, usize), String> {
    if let Some(known) = kept.images.get(workload) {
        return Ok(*known);
    }
    let downloaded = download(settings, workload, true)?;
    let budget = capacity::checkpoints(
        &shared.survey,
        capacity::per_unit_bytes(
            validate::declared_memory_pages(&downloaded)
                .unwrap_or_else(|| validate::Limits::default().max_memory_pages),
        ),
    );

    let bytes: &'static [u8] = Box::leak(downloaded.into_boxed_slice());
    let image: &'static Image<'static> = Box::leak(Box::new(
        image::decode(bytes).map_err(|e| format!("could not decode the module: {e}"))?,
    ));
    kept.images.insert(workload.to_owned(), (image, budget));
    Ok((image, budget))
}

fn download(settings: &Volunteer<'_>, workload: &str, disputable: bool) -> Result<Vec<u8>, String> {
    let suffix = if disputable { "?form=dispute" } else { "" };
    let fetched = client::get(&format!("{}/api/module/{workload}{suffix}", settings.base))?;
    if fetched.status != 200 {
        return Err(format!("module {workload}: {}", fetched.status));
    }
    Ok(fetched.body)
}

/// Bytes, for a person reading one line of startup output.
///
/// The float is for the printing and nothing else. Every decision in this file is made on the
/// integers — `float_cmp` is denied in this workspace for a reason, and a size in a header is the
/// one place a rounded number is the right answer.
#[allow(clippy::cast_precision_loss)]
fn human(bytes: u64) -> String {
    const UNITS: [(&str, u64); 3] = [
        ("GiB", 1024 * 1024 * 1024),
        ("MiB", 1024 * 1024),
        ("KiB", 1024),
    ];
    for (name, size) in UNITS {
        if bytes >= size {
            return format!("{:.1} {name}", bytes as f64 / size as f64);
        }
    }
    format!("{bytes} B")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_are_printed_for_a_person_rather_than_a_machine() {
        assert_eq!(human(0), "0 B");
        assert_eq!(human(512), "512 B");
        assert_eq!(human(1536), "1.5 KiB");
        assert_eq!(human(256 * 1024 * 1024), "256.0 MiB");
        assert_eq!(human(3 * 1024 * 1024 * 1024 / 2), "1.5 GiB");
    }
}
