//! Settling a disagreement by asking the two parties, rather than by doing their work.
//!
//! This is the piece the project is named for. Everything else in `coordinator/` is ordinary
//! work-queue machinery; this is where the coordinator refuses to re-run a computation and
//! makes the two volunteers argue instead.
//!
//! # The shape of it
//!
//! [`cairn_runtime::dispute::resolve`] is a synchronous driver: it calls `root_at(step)` on two
//! [`Claimant`]s in a loop. The parties here are **web clients that poll**, so the two halves
//! meet at a [`Desk`] — a one-slot mailbox with a condition variable. The referee thread blocks
//! in `root_at`; the HTTP handler picks the question up on one request and drops the answer in
//! on the next.
//!
//! That inversion is the whole trick, and it is why `resolve` needed no change to be driven
//! across a network. A party that stops polling stops answering, the wait times out, and the
//! protocol's existing [`Absent`] path attributes the silence to the right party.
//!
//! # What the referee does and does not execute
//!
//! It executes **one instruction**. Not the unit — the instruction that bisection identified.
//! To do that it needs the machine state immediately before it, which it does not have and must
//! not compute: reaching step *n* costs `O(n)`, which is the cost this exists to avoid. So a
//! party hands the state over as a witness ([`cairn_runtime::wire`]), and
//! [`cairn_runtime::dispute::adjudicate`] refuses it unless it reconstructs the root bisection
//! already established. A fabricated witness cannot decide a dispute; it can only be refused.
//!
//! # What this convicts, and what it does not
//!
//! Both parties answer by replaying **the same bytes on the same input under the same
//! deterministic interpreter**. An honest party's replay therefore always reproduces the truth,
//! whatever its own engine did earlier. So bisection converges on a disagreement only when a
//! party *lies during the dispute* — which is exactly the party with something to hide, and
//! exactly the case ADR-0001's economics are about.
//!
//! A party whose original answer was merely *wrong* — a broken engine, a miscompiled build —
//! replays honestly, agrees with the other party, and bisection reports
//! [`DisputeError::NoDisagreement`]. Nobody is convicted, because nobody lied.
//!
//! That case used to cost a full interpreted re-execution. It no longer does. **The answer is
//! part of the committed state**, so a trace the two parties agree on *determines* what the
//! answer was: one witness at the final step, checked against a root both of them already
//! committed to, names the wrong result by comparing two digests. See
//! [ADR-0012](../../docs/adr/0012-the-answer-is-part-of-the-committed-state.md) and
//! [`settle_by_agreed_answer`].

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use cairn_runtime::dispute::{self, Absent, Claimant, DisputeError, Judgment, Party, Step};
use cairn_runtime::engine::image;
use cairn_runtime::engine::machine::{Limits, Machine};
use cairn_runtime::merkle::Hash;
use cairn_runtime::wire;

/// The protocol's vocabulary, and the one implementation of how a party answers it.
///
/// Re-exported rather than restated. A coordinator with its own idea of what an honest answer
/// looks like would be a second implementation of consensus-critical code, and two that disagree
/// do not produce a bug report — they convict an honest volunteer.
pub use cairn_runtime::dispute::{answer as answer_honestly, Answer, Question};

/// How long a party has to answer one question before it is treated as gone.
///
/// A volunteer polls; this has to cover a poll interval plus the replay the answer costs, and a
/// replay is an *interpreted* execution — 37×–142× slower than the engine the work was
/// originally done on ([ADR-0008]). Generous rather than tight: convicting a slow volunteer for
/// being slow is the failure mode this project cares most about avoiding.
///
/// [ADR-0008]: ../../docs/adr/0008-a-dispute-costs-an-interpreted-re-execution.md
pub const DEFAULT_PATIENCE: Duration = Duration::from_secs(60);

/// One party's mailbox: at most one outstanding question, at most one answer.
///
/// # Why a condition variable and not a channel
///
/// The referee must be able to *give up*. A channel receive with a timeout would do, but the
/// desk also has to be readable without consuming — an HTTP GET that showed a party its question
/// and then lost it if the connection dropped would strand the dispute. So the question stays
/// put until it is answered or abandoned, and polling it is free.
pub struct Desk {
    slot: Mutex<Slot>,
    bell: Condvar,
}

#[derive(Default)]
struct Slot {
    question: Option<Question>,
    answer: Option<Answer>,
    /// Increments per question. An answer quoting a stale token is refused, so a slow party's
    /// reply to a question the referee has already given up on cannot be counted as a reply to
    /// the next one — which would let a party answer a question it was never asked.
    token: u64,
}

impl Default for Desk {
    fn default() -> Self {
        Self::new()
    }
}

impl Desk {
    /// An idle desk.
    #[must_use]
    pub fn new() -> Self {
        Self {
            slot: Mutex::new(Slot::default()),
            bell: Condvar::new(),
        }
    }

    /// Put a question and block until it is answered or `patience` runs out.
    ///
    /// `None` means the party did not answer, which the protocol treats as losing by default.
    fn ask(&self, question: Question, patience: Duration) -> Option<Answer> {
        let mut slot = self.slot.lock().ok()?;
        slot.token = slot.token.wrapping_add(1);
        let token = slot.token;
        slot.question = Some(question);
        slot.answer = None;
        self.bell.notify_all();

        let (mut slot, timeout) = self
            .bell
            .wait_timeout_while(slot, patience, |s| s.answer.is_none() && s.token == token)
            .ok()?;

        // Retract the question either way: a party polling after a timeout should be told there
        // is nothing outstanding rather than be sent off to compute an answer nobody wants.
        slot.question = None;
        if timeout.timed_out() {
            return None;
        }
        slot.answer.take()
    }

    /// What this party is being asked, with the token its answer must quote.
    #[must_use]
    pub fn pending(&self) -> Option<(u64, Question)> {
        let slot = self.slot.lock().ok()?;
        slot.question.map(|q| (slot.token, q))
    }

    /// Deliver an answer. `false` if it does not match the outstanding question.
    #[must_use]
    pub fn reply(&self, token: u64, answer: Answer) -> bool {
        let Ok(mut slot) = self.slot.lock() else {
            return false;
        };
        if slot.token != token || slot.question.is_none() {
            return false;
        }
        slot.answer = Some(answer);
        self.bell.notify_all();
        true
    }
}

/// A party reached through a desk. The bridge between a blocking protocol and a polling client.
struct Remote {
    desk: Arc<Desk>,
    party: Party,
    patience: Duration,
    log: Arc<Mutex<Log>>,
}

impl Claimant for Remote {
    fn root_at(&mut self, step: Step) -> Result<Option<Hash>, Absent> {
        let answer = self
            .desk
            .ask(Question::Root { step: step.get() }, self.patience);
        let root = match answer {
            Some(Answer::Root(root)) => root,
            // A party that answers the wrong kind of question is as much use as one that
            // answers none. Both mean the referee cannot proceed with it.
            _ => return Err(Absent),
        };
        if let Ok(mut log) = self.log.lock() {
            log.transcript.push(Utterance {
                party: self.party,
                step: step.get(),
                root,
            });
        }
        Ok(root)
    }
}

/// One question and its answer, kept so the bisection can be watched rather than believed.
///
/// This is the reason the transcript exists at all: the project's central claim is that a
/// dispute of any size costs a few dozen messages, and a reader should be able to count them.
#[derive(Debug, Clone)]
pub struct Utterance {
    /// Who was asked.
    pub party: Party,
    /// The step they were asked about.
    pub step: u64,
    /// What they claimed. `None` is "my execution had ended by then", which is an answer.
    pub root: Option<Hash>,
}

/// How a dispute came out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Conclusion {
    /// Bisection found the instruction, the referee executed it, and one party's claim about
    /// what it produces was false.
    ///
    /// The whole mechanism, exercised. The cost was `rounds` messages and one instruction.
    Convicted {
        /// The party whose claim the disputed instruction refuted.
        liar: Party,
        /// The instruction that first separated the two executions.
        divergence: u64,
        /// Messages exchanged.
        rounds: u32,
    },
    /// A party stopped answering. It loses by default — a materially lighter penalty than a
    /// proven false claim, because volunteers close laptops.
    Abandoned {
        /// The party that went quiet.
        by: Party,
        /// Messages exchanged before it did.
        rounds: u32,
    },
    /// Both parties' claims about the disputed instruction were false.
    ///
    /// Nobody wins and the unit goes back to the queue. Not reachable by accident: it means two
    /// parties independently lied about the same instruction and disagreed about how.
    BothWrong {
        /// Where they were both wrong.
        divergence: u64,
        /// Messages exchanged.
        rounds: u32,
    },
    /// The parties' replays agreed at every step, so nobody lied — and the trace they agreed
    /// on named the answer.
    ///
    /// The cheap resolution of the non-adversarial case. Settled by comparing digests against a
    /// root both parties committed to, with **nothing executed by anybody**.
    AgreedOnTrace {
        /// Whose submitted result contradicted the trace they both agreed on. `None` means
        /// neither matched, which takes two parties agreeing on a trace and both misreporting
        /// its answer.
        wrong: Option<Party>,
        /// Messages exchanged reaching that agreement.
        rounds: u32,
    },
    /// The interactive protocol could not decide it, so re-execution did.
    ///
    /// The ordinary case is [`DisputeError::NoDisagreement`]: the parties' replays agree, so
    /// nobody lied and the original disagreement was not reproducible. See the module docs.
    FellBack {
        /// Why bisection did not settle it, in the protocol's own words.
        why: String,
        /// What re-execution concluded.
        verdict: String,
    },
}

impl Conclusion {
    /// Messages the interactive protocol exchanged, for anyone comparing that against `log₂(n)`.
    #[must_use]
    pub const fn rounds(&self) -> u32 {
        match self {
            Self::Convicted { rounds, .. }
            | Self::Abandoned { rounds, .. }
            | Self::BothWrong { rounds, .. }
            | Self::AgreedOnTrace { rounds, .. } => *rounds,
            Self::FellBack { .. } => 0,
        }
    }
}

/// What is known about a dispute so far.
#[derive(Default)]
pub struct Log {
    /// Every question and answer, in order.
    pub transcript: Vec<Utterance>,
    /// `None` while the referee is still working.
    pub conclusion: Option<Conclusion>,
    /// The answer the referee is prepared to stand behind, once there is one.
    pub output: Option<Vec<u8>>,
}

/// Two volunteers, one work unit, and the argument between them.
pub struct Dispute {
    /// The unit under dispute.
    pub unit: usize,
    /// The two volunteers, in the order [`Party::First`] and [`Party::Second`] name them.
    pub parties: [String; 2],
    /// What each of them originally answered.
    pub outputs: [Vec<u8>; 2],
    /// Which workload, so a party can be told what to replay.
    pub workload: String,
    desks: [Arc<Desk>; 2],
    log: Arc<Mutex<Log>>,
}

impl Dispute {
    /// This worker's desk, if it is a party here.
    #[must_use]
    pub fn desk_for(&self, worker: &str) -> Option<&Arc<Desk>> {
        let index = self.parties.iter().position(|p| p == worker)?;
        self.desks.get(index)
    }

    /// Everything known so far. Safe to read while the referee is still working.
    pub fn log(&self) -> std::sync::MutexGuard<'_, Log> {
        self.log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Whether the referee has finished.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.log().conclusion.is_some()
    }
}

/// Open a dispute and start the referee working on it.
///
/// Returns immediately: the argument runs on its own thread, because both halves of it block —
/// the referee waiting for answers, the HTTP handlers waiting for requests — and neither may
/// hold the coordinator's lock while it does.
///
/// `module` must be the **dispute-path** binary, not the one the volunteers ran. The two are
/// different programs with different instruction counts, and "step 40,000" only names a state
/// if both parties are replaying the same bytes.
#[must_use]
pub fn open(
    unit: usize,
    workload: String,
    parties: [String; 2],
    outputs: [Vec<u8>; 2],
    module: Arc<Vec<u8>>,
    input: Vec<u8>,
    patience: Duration,
) -> Arc<Dispute> {
    let dispute = Arc::new(Dispute {
        unit,
        parties,
        outputs,
        workload,
        desks: [Arc::new(Desk::new()), Arc::new(Desk::new())],
        log: Arc::new(Mutex::new(Log::default())),
    });

    let referee = Arc::clone(&dispute);
    // Detached. A dispute outlives the request that created it, and there is nobody to join it:
    // its result is read out of the log by whoever asks next.
    drop(std::thread::spawn(move || {
        let (conclusion, output) = preside(&referee, &module, &input, patience);
        let mut log = referee
            .log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        log.conclusion = Some(conclusion);
        log.output = output;
    }));

    dispute
}

/// Run the whole argument and say how it came out.
fn preside(
    dispute: &Dispute,
    module: &[u8],
    input: &[u8],
    patience: Duration,
) -> (Conclusion, Option<Vec<u8>>) {
    let Ok(image) = image::decode(module) else {
        return (
            Conclusion::FellBack {
                why: "the disputed module could not be decoded".to_owned(),
                verdict: "no verdict is possible".to_owned(),
            },
            None,
        );
    };

    let fallback = |why: String| {
        let (verdict, output) = by_re_execution(&image, input, &dispute.outputs);
        (Conclusion::FellBack { why, verdict }, output)
    };

    // The parties are asked how long their executions were, because the coordinator does not
    // know and has no cheap way to find out.
    let Some(length) = agreed_length(dispute, patience) else {
        return fallback("neither party would say how long its execution was".to_owned());
    };

    let mut first = remote(dispute, Party::First, patience);
    let mut second = remote(dispute, Party::Second, patience);

    let verdict = match dispute::resolve(&mut first, &mut second, Step::new(length)) {
        Ok(verdict) => verdict,
        // The parties agree at every step. Nobody lied, so there is nobody to convict — but
        // the trace they agree on says what the answer was, because the answer is committed.
        Err(DisputeError::NoDisagreement) => {
            return settle_by_agreed_answer(dispute, &image, length, patience)
                .unwrap_or_else(|| fallback("the parties agree, and no usable witness of the agreed final state was supplied".to_owned()));
        }
        Err(DisputeError::Abandoned { by, rounds, .. }) => {
            // A definite outcome of the interactive protocol, not a failure of it. The party
            // still answering is believed, which is what "loses by default" means.
            let survivor = match by {
                Party::First => 1,
                Party::Second => 0,
            };
            let output = dispute.outputs.get(survivor).cloned();
            return (Conclusion::Abandoned { by, rounds }, output);
        }
        Err(e) => return fallback(e.to_string()),
    };

    // The state to execute from has to come from a party. Either will do: they committed to the
    // same root there, and a witness that does not reconstruct it is refused.
    let Some(witness) = collect_witness(dispute, verdict.divergence, patience) else {
        return fallback(format!(
            "neither party supplied a usable state witness at {}",
            verdict.divergence
        ));
    };

    match dispute::adjudicate(&image, &verdict, &witness, input, Limits::default()) {
        Ok(Judgment::Guilty { liar }) => {
            let honest = match liar {
                Party::First => 1,
                Party::Second => 0,
            };
            (
                Conclusion::Convicted {
                    liar,
                    divergence: verdict.divergence.get(),
                    rounds: verdict.rounds,
                },
                dispute.outputs.get(honest).cloned(),
            )
        }
        Ok(Judgment::BothWrong { .. }) => (
            Conclusion::BothWrong {
                divergence: verdict.divergence.get(),
                rounds: verdict.rounds,
            },
            None,
        ),
        // Bisection only converges where the two claims differ, so one of them must match. This
        // means the verdict and the witness describe different disputes, which is a bug here.
        Ok(Judgment::Inconsistent) => fallback(
            "the adjudicator found both claims correct, which the bisection rules out".to_owned(),
        ),
        Err(e) => fallback(e.to_string()),
    }
}

fn remote(dispute: &Dispute, party: Party, patience: Duration) -> Remote {
    let index = match party {
        Party::First => 0,
        Party::Second => 1,
    };
    Remote {
        desk: dispute
            .desks
            .get(index)
            .map_or_else(|| Arc::new(Desk::new()), Arc::clone),
        party,
        patience,
        log: Arc::clone(&dispute.log),
    }
}

/// The range to bisect over: the longer of the two executions, as the parties report them.
///
/// The longer, so a party that stopped early shows up as disagreeing from the first step past
/// its end rather than being quietly excluded. One party answering is enough to proceed.
fn agreed_length(dispute: &Dispute, patience: Duration) -> Option<u64> {
    let mut longest = None;
    for desk in &dispute.desks {
        if let Some(Answer::Length(n)) = desk.ask(Question::Length, patience) {
            longest = Some(longest.map_or(n, |m: u64| m.max(n)));
        }
    }
    longest.filter(|n| *n > 0)
}

/// Ask each party in turn for the state at `step`, taking the first one that parses.
///
/// Parsing is all that is checked here. Whether the witness describes the *agreed* state is
/// [`dispute::adjudicate`]'s single comparison, and it is what makes accepting one from an
/// interested party safe.
fn collect_witness(
    dispute: &Dispute,
    step: Step,
    patience: Duration,
) -> Option<cairn_runtime::engine::machine::Witness> {
    for desk in &dispute.desks {
        if let Some(Answer::Witness(bytes)) =
            desk.ask(Question::Witness { step: step.get() }, patience)
        {
            if let Ok(witness) = wire::decode(&bytes) {
                return Some(witness);
            }
        }
    }
    None
}

/// Settle a dispute the parties agree about, without executing anything.
///
/// # Why this is possible at all
///
/// Reaching here means bisection found no disagreement: both parties replayed honestly and
/// committed to the same root at every step. Under a deterministic replay that is the ordinary
/// non-adversarial case — one of them returned a wrong *answer* without lying about the
/// *execution*, which is what a broken engine or faulty memory produces.
///
/// The answer is part of the committed state
/// ([ADR-0012](../../docs/adr/0012-the-answer-is-part-of-the-committed-state.md)), so the root
/// the parties already agreed on at the final step **determines** what the answer was. One
/// party supplies the final state as a witness, the witness is checked against that root the
/// same way any other witness is, and then it is two hash comparisons.
///
/// **The coordinator executes nothing.** Not the unit, and — unlike a conviction — not even one
/// instruction. Before the answer was committed this case cost a full interpreted re-execution,
/// which is the most expensive path the system had.
///
/// `None` if the agreed root cannot be established or no party supplies a matching witness, in
/// which case the caller falls back.
fn settle_by_agreed_answer(
    dispute: &Dispute,
    image: &image::Image<'_>,
    length: u64,
    patience: Duration,
) -> Option<(Conclusion, Option<Vec<u8>>)> {
    // Taken from the transcript rather than asked for again. `resolve` already put this question
    // to both parties and got the same answer twice; asking a third time would charge a party
    // another full replay to learn something already written down.
    let (agreed, rounds) = {
        let log = dispute.log();
        let root = log
            .transcript
            .iter()
            .find(|u| u.step == length)
            .and_then(|u| u.root)?;
        (
            root,
            u32::try_from(log.transcript.len()).unwrap_or(u32::MAX),
        )
    };

    let witness = collect_witness(dispute, Step::new(length), patience)?;
    // The same check that makes any witness trustworthy: it must reconstruct the root the
    // parties committed to. A party that fabricates a final state to make its own answer look
    // right fails here.
    if witness.commitment().root() != agreed {
        return None;
    }
    // The image is unused for this decision and is taken to keep the signature honest about
    // what a caller must have: a coordinator that could not decode the module could not have
    // opened the dispute.
    let _ = image;

    let matches = |output: Option<&Vec<u8>>| {
        output.is_some_and(|o| cairn_runtime::state::hash_output(o) == witness.output)
    };
    let first_ok = matches(dispute.outputs.first());
    let second_ok = matches(dispute.outputs.get(1));

    let (wrong, output) = match (first_ok, second_ok) {
        (true, false) => (Some(Party::Second), dispute.outputs.first().cloned()),
        (false, true) => (Some(Party::First), dispute.outputs.get(1).cloned()),
        // Both parties agreed on a trace and both misreported what it answered. Nobody's result
        // is accepted; the unit goes back to the queue.
        (false, false) => (None, None),
        // Unreachable: they were routed here because their outputs differ, so they cannot both
        // hash to the same digest.
        (true, true) => return None,
    };

    Some((Conclusion::AgreedOnTrace { wrong, rounds }, output))
}

/// Settle a disagreement by executing the unit once, here.
///
/// # Read this before quoting anything about how disputes are resolved
///
/// **This is the second route, not a hole where the first one should be.** It is ordinary
/// replication, done by the coordinator instead of by a third volunteer, and it costs a full
/// interpreted execution. It is reached in two situations, and both are legitimate:
///
/// **A party cannot argue.** Answering a challenge means producing a state root, and no engine
/// outside this repository can — a browser volunteer is fast and blind ([ADR-0005]). Bisecting
/// against one would time it out and convict it for silence, so the referee does the work
/// itself. This is the ordinary case in a network of browser volunteers, and it is what such a
/// volunteer is owed.
///
/// **Nobody lied.** Both parties replayed honestly, agreed at every step, and bisection reports
/// [`DisputeError::NoDisagreement`]. Bisection catches *liars*, which is what the economics are
/// about; it does not identify which of two honest-but-differing answers is right, because under
/// a deterministic replay that case does not survive to be bisected.
///
/// What would be dishonest is *replaying both sides here and calling it arbitration* — that
/// looks like the mechanism working while being the coordinator doing both parties' work, which
/// is the exact cost the design exists to avoid. So this does the cheap honest thing and the
/// verdict says which route produced it.
///
/// Returns the verdict in words and the answer the referee is prepared to stand behind.
///
/// [ADR-0005]: ../../docs/adr/0005-the-fast-path-cannot-snapshot.md
#[must_use]
pub fn by_re_execution(
    image: &image::Image<'_>,
    input: &[u8],
    outputs: &[Vec<u8>; 2],
) -> (String, Option<Vec<u8>>) {
    // Judged against the unit *as assigned*. Judging against anything else produces a
    // well-formed verdict for a different question, which is a mistake worth remembering
    // because it looks like an answer.
    let Some(truth) = honest_output(image, input) else {
        return (
            "the unit could not be executed for adjudication".to_owned(),
            None,
        );
    };

    let matches_first = outputs.first().is_some_and(|o| *o == truth);
    let matches_second = outputs.get(1).is_some_and(|o| *o == truth);

    let verdict = match (matches_first, matches_second) {
        (true, false) => format!("the {} was wrong", Party::Second),
        (false, true) => format!("the {} was wrong", Party::First),
        (false, false) => "neither party's result matches the unit as assigned".to_owned(),
        (true, true) => {
            "inconsistent — both results match, so this was not a disagreement".to_owned()
        }
    };

    (
        format!("{verdict} (settled by re-execution, not by bisection)"),
        Some(truth),
    )
}

/// Execute the unit as assigned, for the answer the referee is prepared to stand behind.
fn honest_output(image: &image::Image<'_>, input: &[u8]) -> Option<Vec<u8>> {
    let mut machine = Machine::new(image, input.to_vec(), Limits::default()).ok()?;
    machine.run().ok().map(|trace| trace.output)
}
