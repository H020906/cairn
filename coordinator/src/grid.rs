//! What the coordinator knows and what it decides, with no HTTP in it.
//!
//! Every decision Cairn's coordinator makes lives here: which unit to hand a volunteer, what
//! to do with a result that comes back, and when two results mean a dispute rather than a
//! coincidence. The HTTP layer in [`crate::api`] does nothing but translate, and the argument
//! itself is [`crate::dispute`].
//!
//! The split is not tidiness. A coordinator is the one component that can convict an honest
//! volunteer, so its decisions have to be testable without a socket — and everything in this
//! file is reachable from a unit test that never opens one.
//!
//! # What this is not
//!
//! **There is no database, and after [ADR-0014] there is not going to be one.** State is in
//! memory; what makes it survive a restart is an append-only journal in [`crate::journal`],
//! replayed through [`Grid::restore`]. Every read in this file is a linear scan of a `Vec`, so
//! there is nothing here for a query engine to do — persistence is how the memory is rebuilt,
//! not where the data lives.
//!
//! Two things are still absent and they are the reason
//! [ADR-0002](../../docs/adr/0002-language-boundaries.md) wanted Spring: **transactions across
//! more than one writer**, which a single-process coordinator does not have, and **a store more
//! than one coordinator can share**, which is what a real deployment would need. See
//! [ADR-0010](../../docs/adr/0010-the-referee-executes-so-the-coordinator-is-rust.md).
//!
//! [ADR-0014]: ../../docs/adr/0014-the-coordinator-keeps-a-log-not-a-database.md
//!
//! **There are canaries and a reputation now** ([ADR-0015]), and the warning this paragraph used
//! to carry still applies to what they are allowed to be. ADR-0001 asks for "a per-worker
//! posterior on returns correct results", so [`crate::reputation`] computes exactly that and
//! invents nothing; the thresholds it needs are dials with stated defaults, like `--replicate`,
//! rather than constants somebody believed in.
//!
//! The rule that came out of building it: **a canary is only as true as the unit it was copied
//! from.** A unit accepted after a single execution is one volunteer's word, and minting a canary
//! from one promotes a cheat's answer to ground truth — after which honest volunteers are marked
//! as cheats for being right. Sources must be corroborated, which means `c` and `r` are not the
//! independent terms ADR-0001's cost model adds together.
//!
//! **Nothing here is a penalty, and that is still deliberate.** A volunteer proven wrong is
//! checked harder and nothing else. [`crate::dispute`] names who lied and who went quiet, and
//! ADR-0001 wants those to cost differently — they do, in the posterior — but *excluding* a
//! volunteer is a decision with consequences for a real person and it needs an operator.
//!
//! [ADR-0015]: ../../docs/adr/0015-canaries-are-what-catch-a-cheat.md

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cairn_runtime::canon::{self, Config};
use cairn_runtime::engine::image;
use cairn_runtime::validate;

use crate::dispute::{self, Conclusion, Dispute, ReExecution};
use crate::journal::Entry;
use crate::reputation::{Reputation, Standing};
use cairn_runtime::dispute::Party;

/// A work unit's identity: the BLAKE3 hash of the canonical module bytes.
///
/// Of the *canonical* bytes, not the submitted ones. Two builds of a workload that differ only
/// in a producers section instrument to identical bytes and are therefore the same unit —
/// which is why [`canon`] drops custom sections.
pub type UnitId = String;

/// How many volunteers must independently agree before a result is accepted.
///
/// One is the point of the project: accept after a single execution and settle the rare
/// disagreement. Replication above one exists only as the dial ADR-0001 calls `r`, applied to
/// a sampled fraction of units rather than all of them.
pub const DEFAULT_QUORUM: usize = 1;

/// Fraction of units handed to a second volunteer as a spot check, in hundredths.
///
/// ADR-0001's `r`. Ten percent is the figure that ADR's cost model assumes, and it is policy
/// rather than measurement — choosing differently moves the cost result, which
/// `docs/benchmarks.md` says where it reports it.
pub const DEFAULT_REPLICATION_PERCENT: u32 = 10;

/// How long a volunteer has to return a result before the unit is offered to somebody else.
///
/// Not a heartbeat and not a renewable lease — a volunteer that vanishes simply stops mattering
/// after this long. A real coordinator wants both; this one wants to not hand out the same unit
/// forever.
pub const DEFAULT_LEASE: Duration = Duration::from_secs(120);

/// A registered workload: the canonical bytes every volunteer receives, and its identity.
pub struct Workload {
    /// The instrumented module. Handed to volunteers unchanged.
    pub module: Vec<u8>,
    /// The fully instrumented module, kept for the day somebody disputes a result.
    ///
    /// Built at registration rather than on demand, because building it is deterministic and
    /// doing it under the pressure of an open dispute is a worse time to discover a problem.
    ///
    /// Shared rather than owned because a dispute runs on its own thread and needs it for as
    /// long as the argument lasts.
    pub disputable: Arc<Vec<u8>>,
    /// Human-readable, for the dashboard that does not exist yet.
    pub name: String,
}

/// One piece of work: a workload and the input it is to be run on.
pub struct Unit {
    /// Which registered workload this unit runs.
    pub workload: UnitId,
    /// The bytes handed to `cairn.input`.
    pub input: Vec<u8>,
    /// Results returned so far, by worker.
    pub results: Vec<Submission>,
    /// How many agreeing results this unit needs.
    pub quorum: usize,
    /// Who has been given this unit, and until when.
    ///
    /// **Expired entries are kept**, and that is the point rather than an oversight. A lease is
    /// two things at once: a *reservation*, which [`Grid::lease`] reads by expiry, and the
    /// *evidence* that a worker was assigned this unit, which [`Grid::submit_result`] reads by
    /// membership. Deleting an expired lease would throw away the second along with the first,
    /// and the volunteer that comes back with a good answer a moment late — or after a restart,
    /// where every restored lease is expired by construction — would be told it was never given
    /// the work. So expiry is applied where it is read.
    leases: Vec<(String, Instant)>,
    /// Where this unit has got to.
    pub outcome: Outcome,
    /// Set when this unit is a check rather than science. See [`Grid::mint_canary`].
    pub canary: Option<Canary>,
}

/// A unit the coordinator already knows the answer to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Canary {
    /// What the source unit was accepted as. A different answer is a wrong answer, established
    /// without a second volunteer and without a dispute.
    pub expected: Vec<u8>,
    /// The decided unit this was copied from.
    ///
    /// Kept so that a worker is never handed a canary derived from a unit it answered itself,
    /// which would be asking somebody to confirm their own work.
    pub source: usize,
}

/// One volunteer's answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Submission {
    /// Who answered. One volunteer, one vote.
    pub worker: String,
    /// The bytes the workload wrote through `cairn.output`.
    pub output: Vec<u8>,
    /// What the volunteer says the unit cost, if the unit was prepared to report it.
    ///
    /// Advisory. A volunteer could report anything, and nothing is decided on it — it exists so
    /// the network can *account* for contributed work, which is what
    /// [ADR-0009](../../docs/adr/0009-metering-through-a-global-the-engines-disagree.md) made
    /// possible. Making it load-bearing would need it to be part of the disputed state.
    pub fuel: Option<u64>,
    /// Whether this volunteer can be a party to an interactive dispute.
    ///
    /// # Why a volunteer is allowed to say no
    ///
    /// Answering a challenge means producing the state root after *n* instructions, and no
    /// WebAssembly engine outside this repository can — the operand stack and the frame chain
    /// are not things an embedder gets to read ([ADR-0005]). A browser volunteer executes at
    /// full speed and cannot say a word about *how* it got there. That is by design, and it is
    /// most of why this project exists.
    ///
    /// So a volunteer declares, and the coordinator believes it. Assuming otherwise and
    /// challenging a browser would time it out and convict it by default — **an honest
    /// volunteer convicted for running in a browser**, which is the exact failure this codebase
    /// is organised to prevent.
    ///
    /// [ADR-0005]: ../../docs/adr/0005-the-fast-path-cannot-snapshot.md
    pub bisects: bool,
}

/// Where a unit has got to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Waiting for enough agreeing results.
    Open,
    /// Enough volunteers agreed. This is what happens to almost every unit.
    Accepted {
        /// The answer every volunteer who ran it agreed on.
        output: Vec<u8>,
    },
    /// Two volunteers disagreed and are arguing about it. The argument may still be running.
    ///
    /// Its state lives in a [`Dispute`]; fetch it with [`Grid::dispute`]. It is not copied in
    /// here because a dispute is a *process* whose transcript grows, and a snapshot of one
    /// stored in the unit would be stale the moment it was taken.
    Disputed {
        /// Index into the grid's disputes.
        dispute: usize,
    },
    /// Two volunteers disagreed and the referee settled it by re-executing the unit itself.
    ///
    /// The fallback route, taken when the parties cannot argue — see [`Submission::bisects`].
    Settled {
        /// What the referee concluded. `Display` renders it as the sentence this used to be.
        verdict: dispute::ReExecution,
        /// The volunteers whose results the referee's own execution contradicts, by name.
        ///
        /// **The whole point of ADR-0017.** [`dispute::ReExecution`] names *parties*, which are
        /// positions in an argument; reputation and the journal need *workers*, and this is the
        /// only place both are in scope at once. Leaving the translation to a later reader is
        /// how the finding got lost the first time.
        refuted: Vec<String>,
        /// The answer, when the referee could establish one.
        output: Option<Vec<u8>>,
    },
}

/// What a concluded dispute costs a volunteer.
///
/// Three of them rather than a boolean, because there are three genuinely different things the
/// coordinator can know: that a party corrupted its own replay, that a party stopped answering,
/// and that a party's *result* was wrong without any evidence about why. The last one arrived
/// with ADR-0017; before it, the middle one was doing double duty and the third was not recorded
/// at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Charge {
    /// Lost a bisection. The heaviest thing this project can establish about a volunteer.
    Lie,
    /// Stopped answering, or honestly replayed a trace that contradicts its own reported answer.
    Silence,
    /// The referee re-executed the unit and this volunteer's result is not the answer.
    Refutation,
}

/// Why a submission was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// No unit with that index.
    UnknownUnit,
    /// The worker holds no lease on this unit. Prevents a result arriving for work that was
    /// never assigned, which is not an attack so much as a bookkeeping hole.
    NotLeased,
    /// This worker already answered. One volunteer, one vote.
    AlreadyAnswered,
    /// The unit is decided; a late result changes nothing.
    Closed,
}

impl core::fmt::Display for Refusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnknownUnit => write!(f, "no such unit"),
            Self::NotLeased => write!(f, "this worker holds no lease on that unit"),
            Self::AlreadyAnswered => write!(f, "this worker has already answered"),
            Self::Closed => write!(f, "that unit is already decided"),
        }
    }
}

/// The whole coordinator.
pub struct Grid {
    workloads: HashMap<UnitId, Workload>,
    units: Vec<Unit>,
    disputes: Vec<Arc<Dispute>>,
    replication_percent: u32,
    lease: Duration,
    patience: Duration,
    reputation: Reputation,
    /// Leases handed out since this coordinator started. Only ever used to vary the canary roll.
    leases_handed_out: u64,
    /// Disputes already folded into [`Self::reputation`], so nobody is charged twice.
    accounted: HashSet<usize>,
    /// Makes the canary schedule unpredictable to a worker that counts its own leases.
    ///
    /// Per-process and never written down. Recovering it across a restart would let a worker
    /// that had once learned it keep the advantage forever, and losing it costs nothing: what
    /// must be durable is *which* canaries were issued and how they came out, not the coin that
    /// chose them.
    canary_secret: [u8; 32],
}

impl Default for Grid {
    fn default() -> Self {
        Self::new()
    }
}

impl Grid {
    /// An empty grid with the default replication rate and lease.
    #[must_use]
    pub fn new() -> Self {
        Self {
            workloads: HashMap::new(),
            units: Vec::new(),
            disputes: Vec::new(),
            replication_percent: DEFAULT_REPLICATION_PERCENT,
            lease: DEFAULT_LEASE,
            patience: dispute::DEFAULT_PATIENCE,
            reputation: Reputation::default(),
            accounted: HashSet::new(),
            leases_handed_out: 0,
            canary_secret: fresh_secret(),
        }
    }

    /// Judge volunteers under a chosen policy. See [`crate::reputation::Policy`].
    #[must_use]
    pub fn with_reputation(mut self, reputation: Reputation) -> Self {
        self.reputation = reputation;
        self
    }

    /// Replicate this percentage of units to a second volunteer. `0` disables spot checks.
    #[must_use]
    pub const fn with_replication(mut self, percent: u32) -> Self {
        self.replication_percent = percent;
        self
    }

    /// How long a volunteer holds a unit before it is offered to somebody else.
    #[must_use]
    pub const fn with_lease(mut self, lease: Duration) -> Self {
        self.lease = lease;
        self
    }

    /// How long a party to a dispute has to answer one question.
    ///
    /// Tests want milliseconds; a real network wants a minute, because an answer costs an
    /// interpreted replay. See [`dispute::DEFAULT_PATIENCE`].
    #[must_use]
    pub const fn with_patience(mut self, patience: Duration) -> Self {
        self.patience = patience;
        self
    }

    /// Validate, instrument and store a workload. The coordinator's half of the protocol.
    ///
    /// Instrumentation happens **once, here**, and every volunteer runs the resulting bytes
    /// unchanged — which is what lets their hash be the unit's identity. A volunteer that could
    /// rewrite its own work unit would be a volunteer whose result means nothing.
    ///
    /// # Errors
    ///
    /// The workload's own rejection message, if the admission gate refuses it.
    pub fn register(&mut self, name: &str, source: &[u8]) -> Result<UnitId, String> {
        let source = if source.starts_with(b"\0asm") {
            source.to_vec()
        } else {
            wat::parse_bytes(source)
                .map_err(|e| format!("could not assemble: {e}"))?
                .into_owned()
        };

        validate::validate_submitted(&source, validate::Limits::default())
            .map_err(|e| format!("not an admissible Cairn workload: {e}"))?;

        // What volunteers run: determinism only, plus the counter so they can report what the
        // unit cost. See ADR-0009 — this is the honest path's one addition, and it is ~8% on a
        // compiler where the host-call encoding was +540%.
        let module = canon::instrument(
            &source,
            Config {
                meter: canon::Metering::Global,
                ..Config::honest_path()
            },
        )
        .map_err(|e| format!("could not instrument: {e}"))?;

        // What both parties re-run if somebody disputes: everything. A *different program* from
        // the one above, with different instruction counts — which is why a dispute has to name
        // which of the two it is about, and why the parties are handed these bytes rather than
        // the ones they ran.
        let disputable = Arc::new(
            canon::instrument(&source, Config::dispute_path())
                .map_err(|e| format!("could not instrument for dispute: {e}"))?,
        );

        let id = blake3::hash(&module).to_hex().to_string();
        self.workloads.insert(
            id.clone(),
            Workload {
                module,
                disputable,
                name: name.to_owned(),
            },
        );
        Ok(id)
    }

    /// Queue a unit of work.
    ///
    /// # Errors
    ///
    /// If the workload was never registered.
    pub fn submit(&mut self, workload: &str, input: Vec<u8>) -> Result<usize, String> {
        if !self.workloads.contains_key(workload) {
            return Err(format!("no workload {workload}"));
        }
        // The replication rate applied here rather than at dispatch, so a unit's quorum is
        // fixed before anyone asks for it and cannot change under a worker's feet.
        let replicated = self.replication_percent > 0
            && self.units.len() as u32 % 100 < self.replication_percent;

        self.units.push(Unit {
            workload: workload.to_owned(),
            input,
            results: Vec::new(),
            quorum: if replicated { 2 } else { DEFAULT_QUORUM },
            leases: Vec::new(),
            outcome: Outcome::Open,
            canary: None,
        });
        Ok(self.units.len() - 1)
    }

    /// Rebuild a grid from what the journal recorded.
    ///
    /// # Why this replays facts rather than re-making decisions
    ///
    /// The obvious implementation puts every entry back through `register`, `submit` and
    /// `submit_result`, so that a restored grid is provably reachable by ordinary operation.
    /// That is the wrong shape here, and the reason is specific rather than aesthetic:
    /// `submit_result` on the second disagreeing result **opens a live dispute** — it spawns a
    /// referee thread and starts a patience timer against two volunteers who are not connected
    /// yet. Replay would therefore start a fresh argument for every dispute the coordinator ever
    /// had, and lose all of them by timeout, **convicting honest volunteers on startup**.
    ///
    /// So results are recorded rather than judged. The decision was already made once, by the
    /// coordinator that wrote the entry; making it again is not verification, it is a second
    /// chance to get it wrong. Registration is the exception and goes back through the live
    /// path, because re-instrumenting is how a change to the instrumentation pass surfaces as an
    /// id that no longer matches instead of as a grid quietly serving different bytes.
    ///
    /// # What a restart does to an argument
    ///
    /// Voids it. A unit that was in a dispute comes back **`Open` with its results discarded**,
    /// and both parties are eligible for it again. Resuming is not possible — a dispute is a
    /// live protocol with a blocking referee, two mailboxes and two volunteers mid-replay — and
    /// the alternative, timing out whichever party did not come back, would convict a volunteer
    /// for the coordinator's crash. See [`crate::journal`].
    ///
    /// # Errors
    ///
    /// A journal that names a workload or unit that does not exist, or a workload that no longer
    /// registers. All of those mean the file does not describe this build, and coming up with a
    /// grid that is *nearly* the one that died is worse than refusing.
    pub fn restore(&mut self, entries: &[Entry]) -> Result<Restored, String> {
        let mut restored = Restored::default();
        // One reading of the clock for the whole replay. Every lease is restored already expired,
        // so this is a point in the past by the time anybody asks — which is the intent.
        let now = Instant::now();

        for (index, entry) in entries.iter().enumerate() {
            let at = || format!("journal entry {index}");
            match entry {
                Entry::Registered { name, source } => {
                    self.register(name, source)
                        .map_err(|e| format!("{}: {name} no longer registers: {e}", at()))?;
                    restored.workloads += 1;
                }

                Entry::Queued {
                    workload,
                    input,
                    quorum,
                } => {
                    if !self.workloads.contains_key(workload) {
                        return Err(format!("{}: no workload {workload}", at()));
                    }
                    self.units.push(Unit {
                        workload: workload.clone(),
                        input: input.clone(),
                        results: Vec::new(),
                        // Recorded, not recomputed. Restarting with a different `--replicate`
                        // must not change the quorum of a unit volunteers are already working on.
                        quorum: *quorum,
                        leases: Vec::new(),
                        outcome: Outcome::Open,
                        // A canary is minted at lease time and is not queued work, so nothing in
                        // the journal reconstructs one. What survives a restart is its *outcome*
                        // — see `Entry::Canary`.
                        canary: None,
                    });
                    restored.units += 1;
                }

                Entry::Leased { unit, worker } => {
                    let held = self
                        .units
                        .get_mut(*unit)
                        .ok_or_else(|| format!("{}: no unit {unit}", at()))?;
                    held.leases.retain(|(who, _)| who != worker);
                    // **Expired on arrival, deliberately.** A lease is evidence that this worker
                    // was given this unit, which `submit_result` checks by membership, and a
                    // reservation against other workers, which `lease` checks by expiry.
                    // Restoring only the evidence lets the volunteer that was mid-unit return
                    // its answer, while the unit stays available to everybody else — a
                    // reservation for a volunteer that may never come back would delay the unit
                    // by a lease timeout to protect work that is probably gone.
                    held.leases.push((worker.clone(), now));
                    restored.leases += 1;
                }

                Entry::Answered {
                    unit,
                    worker,
                    output,
                    fuel,
                    bisects,
                } => {
                    let held = self
                        .units
                        .get_mut(*unit)
                        .ok_or_else(|| format!("{}: no unit {unit}", at()))?;
                    // Mirrors `submit_result`: answering consumes the lease. Without this a
                    // restored grid would carry a lease for every unit ever handed out.
                    held.leases.retain(|(who, _)| who != worker);
                    held.results.push(Submission {
                        worker: worker.clone(),
                        output: output.clone(),
                        fuel: *fuel,
                        bisects: *bisects,
                    });
                    restored.results += 1;
                }

                Entry::Accepted { unit, output } => {
                    self.units
                        .get_mut(*unit)
                        .ok_or_else(|| format!("{}: no unit {unit}", at()))?
                        .outcome = Outcome::Accepted {
                        output: output.clone(),
                    };
                    restored.decided += 1;
                }

                Entry::Settled {
                    unit,
                    verdict,
                    refuted,
                    output,
                } => {
                    self.units
                        .get_mut(*unit)
                        .ok_or_else(|| format!("{}: no unit {unit}", at()))?
                        .outcome = Outcome::Settled {
                        verdict: *verdict,
                        refuted: refuted.clone(),
                        output: output.clone(),
                    };
                    // Reading a fact back, not re-deciding one — ADR-0014's rule, and this
                    // satisfies it because the referee already executed the unit and already
                    // named these workers. Nothing is re-executed here and no verdict is
                    // recomputed; the entry says who was wrong and this believes it.
                    for worker in refuted {
                        self.reputation.refuted(worker);
                    }
                    restored.decided += 1;
                }

                Entry::Canary { worker, passed } => {
                    if *passed {
                        self.reputation.passed_a_canary(worker);
                    } else {
                        self.reputation.failed_a_canary(worker);
                    }
                    restored.canaries += 1;
                }

                Entry::Disputed { unit, parties } => {
                    let held = self
                        .units
                        .get_mut(*unit)
                        .ok_or_else(|| format!("{}: no unit {unit}", at()))?;
                    // Discarded, not kept. A unit left `Open` while still holding the two
                    // results that disagreed would never be handed out again — `lease` counts
                    // results against the quorum — so it would sit there looking available and
                    // never be worked on.
                    held.results.clear();
                    held.leases.clear();
                    held.outcome = Outcome::Open;
                    restored.voided.push((*unit, parties.clone()));
                }
            }
        }

        Ok(restored)
    }

    /// What the coordinator has observed about its volunteers.
    #[must_use]
    pub const fn reputation(&self) -> &Reputation {
        &self.reputation
    }

    /// Mint a canary for `worker` if it is due one and there is a decided unit to copy.
    ///
    /// # What makes a canary work, and what breaks it
    ///
    /// A canary is a unit whose answer the coordinator already knows, handed out as if it were
    /// new work. It is the only mechanism in Cairn that catches a wrong answer **by itself** —
    /// everything else waits for a second volunteer to disagree, which happens on the fraction
    /// of units that are replicated and never on the rest.
    ///
    /// ADR-0001 states the condition it depends on: canaries must be "drawn from the same
    /// workload and the same input distribution as live units, and must not be reused across
    /// workers". Both are structural here rather than aspirational:
    ///
    /// - **Same distribution**, because a canary *is* a live unit — a decided one, queued again
    ///   with the same workload and the same input. There is no separate canary corpus that
    ///   could drift away from the real one.
    /// - **A fresh index**, because `/api/status` is public and says which units are decided. A
    ///   canary that reused its source's index would announce itself to anybody who looked.
    /// - **Not reused**, because each minted canary is dispatched once, to one worker, and
    ///   `source` records where it came from so the same worker is never given a unit whose
    ///   answer it has already produced.
    ///
    /// **The limit worth stating: the schedule is unpredictable, the content is not.** Two
    /// colluding workers who share inputs and answers defeat this, because one of them has seen
    /// the source unit. ADR-0001 scopes collusion out; this does not fix it.
    ///
    /// # A canary is only as true as the unit it was copied from
    ///
    /// This was found by a test that failed, and it is the sharpest thing in the file. A unit
    /// accepted after a **single** execution — which is almost every unit, and the entire point
    /// of the project — carries one volunteer's word for its answer. Minting a canary from one
    /// of those takes a cheat's wrong answer and promotes it to *the answer the coordinator
    /// knows*, so the next volunteer to be handed that canary is marked as a cheat for being
    /// right. The mechanism would not merely fail to catch cheats; it would launder them into
    /// convicting honest people.
    ///
    /// So a source must be **corroborated**: either two volunteers answered it and agreed, or
    /// every volunteer who answered it was already trusted. See [`Grid::is_corroborated`].
    ///
    /// That has a consequence for ADR-0001's cost model, which adds `c` and `r` as independent
    /// terms: **they are not independent.** Corroboration comes from replication, and trust
    /// comes from canaries, so `r = 0` means no corroborated units, which means no canaries,
    /// which means no worker ever becomes trusted. Replication is what canaries are grounded
    /// in, and a grid that turns it off has turned them off too.
    fn mint_canary(&mut self, worker: &str, now: Instant) -> Option<Assignment> {
        let due = self.reputation.canary_permille(worker);
        if due == 0 {
            return None;
        }

        // Unpredictable to the worker, deterministic to the coordinator. A counter — the idiom
        // `--replicate` uses — would let a worker that counts its own leases know exactly which
        // unit is the checked one, and answer that one honestly. Hashing a per-run secret with
        // the worker's name and its lease number costs one BLAKE3 of 40-odd bytes and removes
        // that. It is not a cryptographic guarantee against an adversary who can read the
        // coordinator's memory, and it is not meant to be.
        let sequence = self.leases_handed_out;
        let mut probe = Vec::with_capacity(48);
        probe.extend_from_slice(&self.canary_secret);
        probe.extend_from_slice(worker.as_bytes());
        probe.extend_from_slice(&sequence.to_le_bytes());
        let rolled = blake3::hash(&probe);
        let bytes = rolled.as_bytes();
        let roll = u32::from_le_bytes(<[u8; 4]>::try_from(bytes.get(..4)?).ok()?) % 1000;
        if roll >= due {
            return None;
        }

        // Corroborated units this worker has not answered. The corroboration is the important
        // half and the reason is in this function's documentation: a canary minted from one
        // volunteer's unconfirmed word promotes that word to ground truth.
        let eligible: Vec<usize> = self
            .units
            .iter()
            .enumerate()
            .filter(|(_, unit)| {
                self.is_corroborated(unit) && !unit.results.iter().any(|r| r.worker == worker)
            })
            .map(|(index, _)| index)
            .collect();
        if eligible.is_empty() {
            return None;
        }
        // Chosen from the same hash, and varied on purpose. Always copying the newest decided
        // unit would hand every worker the same input, and ADR-0001 requires that canaries "not
        // be reused across workers" — two volunteers comparing notes would find one immediately.
        let pick = u32::from_le_bytes(<[u8; 4]>::try_from(bytes.get(4..8)?).ok()?) as usize;
        let index = *eligible.get(pick % eligible.len())?;
        let unit = self.units.get(index)?;
        let Outcome::Accepted { ref output } = unit.outcome else {
            return None;
        };

        let canary = Unit {
            workload: unit.workload.clone(),
            input: unit.input.clone(),
            results: Vec::new(),
            // One answer settles a canary: the coordinator is the second opinion, and it already
            // has it. Replicating a unit whose answer is known would spend a volunteer to learn
            // nothing.
            quorum: 1,
            leases: vec![(worker.to_owned(), now + self.lease)],
            outcome: Outcome::Open,
            canary: Some(Canary {
                expected: output.clone(),
                source: index,
            }),
        };
        let workload = canary.workload.clone();
        let input = canary.input.clone();
        self.units.push(canary);

        Some(Assignment {
            unit: self.units.len() - 1,
            workload,
            input,
        })
    }

    /// Whether this unit's answer is good enough to check somebody else against.
    ///
    /// Two ways to be sure of an answer, and single-execution acceptance is neither of them:
    ///
    /// - **Two volunteers answered and agreed.** That is what `--replicate` buys, and it is the
    ///   only source of ground truth a brand-new grid has.
    /// - **Every volunteer who answered it was already trusted**, which means each of them had
    ///   passed canaries drawn from units corroborated the first way. The chain is grounded.
    ///
    /// Canaries are excluded as sources, so one known answer cannot be copied across the grid
    /// in place of sampling it.
    fn is_corroborated(&self, unit: &Unit) -> bool {
        if unit.canary.is_some() || !matches!(unit.outcome, Outcome::Accepted { .. }) {
            return false;
        }
        unit.results.len() >= 2
            || unit.results.iter().all(|r| {
                matches!(
                    self.reputation.standing(&r.worker),
                    Standing::Trusted { .. }
                )
            })
    }

    /// Fold any newly-finished disputes into the volunteers' records.
    ///
    /// # Why this is swept rather than pushed
    ///
    /// A dispute concludes on its own thread, some seconds after the request that started it
    /// returned, and it has nowhere to push the news: `dispute.rs` knows about arguments and
    /// deliberately not about reputation. So the grid sweeps. It is cheap — a walk of a list
    /// that is short by design, skipping everything already accounted for — and it happens on
    /// the busiest path there is, so a verdict is never long unnoticed.
    ///
    /// A dispute is accounted for **once**. `accounted` is what makes that true, and without it
    /// a worker would be charged for the same lost argument on every lease anybody made.
    fn account_for_finished_disputes(&mut self) {
        for index in 0..self.disputes.len() {
            if self.accounted.contains(&index) {
                continue;
            }
            let Some(dispute) = self.disputes.get(index) else {
                continue;
            };
            let log = dispute.log();
            let Some(conclusion) = log.conclusion.as_ref() else {
                continue;
            };

            let party = |which: Party| -> Option<String> {
                dispute
                    .parties
                    .get(match which {
                        Party::First => 0,
                        Party::Second => 1,
                    })
                    .cloned()
            };

            let charge: Vec<(String, Charge)> = match conclusion {
                // Proven, by executing the instruction they disagreed about. The heaviest thing
                // this project can establish about a volunteer.
                Conclusion::Convicted { liar, .. } => party(*liar)
                    .map(|who| vec![(who, Charge::Lie)])
                    .unwrap_or_default(),
                // Went quiet. ADR-0001 insists this cost materially less than a proven lie, and
                // `Policy::weight_of_silence` is where that lives.
                Conclusion::Abandoned { by, .. } => party(*by)
                    .map(|who| vec![(who, Charge::Silence)])
                    .unwrap_or_default(),
                // Two parties lied about the same instruction and disagreed about how. Not
                // reachable by accident.
                Conclusion::BothWrong { .. } => dispute
                    .parties
                    .iter()
                    .map(|who| (who.clone(), Charge::Lie))
                    .collect(),
                // Nobody lied — both replays agreed, and the trace they agreed on named whose
                // *reported answer* was wrong. A wrong answer honestly replayed is a broken
                // engine, not a liar, so it is charged as a failed check rather than a lie.
                Conclusion::AgreedOnTrace { wrong, .. } => wrong
                    .and_then(party)
                    .map(|who| vec![(who, Charge::Silence)])
                    .unwrap_or_default(),
                // The protocol failing proves nothing about anybody — but the re-execution it
                // fell back to may prove plenty, because the referee ran the unit and holds the
                // answer. **The old comment here said "nothing is proven" and was right only
                // because the verdict was a `String` nothing could read.** `ReExecution::NoAnswer`
                // is the case that really does establish nothing, and it says so itself.
                Conclusion::FellBack { verdict, .. } => verdict
                    .refuted()
                    .iter()
                    .filter_map(|who| party(*who))
                    .map(|who| (who, Charge::Refutation))
                    .collect(),
            };

            drop(log);
            for (who, charge) in charge {
                match charge {
                    Charge::Lie => self.reputation.lied(&who),
                    Charge::Silence => self.reputation.went_silent(&who),
                    Charge::Refutation => self.reputation.refuted(&who),
                }
            }
            self.accounted.insert(index);
        }
    }

    /// A registered workload by id.
    #[must_use]
    pub fn workload(&self, id: &str) -> Option<&Workload> {
        self.workloads.get(id)
    }

    /// One unit by index.
    #[must_use]
    pub fn unit(&self, index: usize) -> Option<&Unit> {
        self.units.get(index)
    }

    /// Every unit, in the order they were queued.
    #[must_use]
    pub fn units(&self) -> &[Unit] {
        &self.units
    }

    /// One dispute by index.
    #[must_use]
    pub fn dispute(&self, index: usize) -> Option<&Arc<Dispute>> {
        self.disputes.get(index)
    }

    /// Every dispute opened so far, running or finished.
    #[must_use]
    pub fn disputes(&self) -> &[Arc<Dispute>] {
        &self.disputes
    }

    /// The dispute this worker is a party to, if it is a party to one that is still running.
    ///
    /// What a polling volunteer asks. Scanning is fine at this scale and keeps a worker from
    /// having to remember anything: a volunteer's entire state is its name.
    #[must_use]
    pub fn dispute_for(&self, worker: &str) -> Option<(usize, &Arc<Dispute>)> {
        self.disputes
            .iter()
            .enumerate()
            .find(|(_, d)| !d.is_finished() && d.desk_for(worker).is_some())
    }

    /// Hand a volunteer something to do, or `None` if there is nothing.
    ///
    /// Skips units this worker has already answered — one volunteer, one vote, and a unit
    /// replicated back to the same machine is not replicated at all. That is the single most
    /// important line in this function: it is what makes a quorum mean two *independent*
    /// executions rather than the same execution counted twice.
    pub fn lease(&mut self, worker: &str, now: Instant) -> Option<Assignment> {
        self.leases_handed_out = self.leases_handed_out.saturating_add(1);
        // Verdicts arrive on their own threads with nowhere to push the news, so the busiest
        // path collects them. See `account_for_finished_disputes`.
        self.account_for_finished_disputes();
        // Checked before real work, so that a canary is not merely one of the units a worker
        // gets but is drawn from the same stream at the same moment. Nothing about the reply
        // distinguishes it.
        if let Some(canary) = self.mint_canary(worker, now) {
            return Some(canary);
        }

        for (index, unit) in self.units.iter_mut().enumerate() {
            if unit.outcome != Outcome::Open {
                continue;
            }
            if unit.results.iter().any(|r| r.worker == worker) {
                continue;
            }
            // Already holds it, and has not run out of time. A worker whose lease *has* expired
            // may take it again — its entry stays as evidence and is refreshed below.
            if unit
                .leases
                .iter()
                .any(|(who, expiry)| who == worker && *expiry > now)
            {
                continue;
            }
            // Live leases plus results already in hand cover the quorum, so nobody else is
            // needed yet. Counted rather than pruned: see the field's documentation.
            let reserved = unit.leases.iter().filter(|(_, e)| *e > now).count();
            if reserved + unit.results.len() >= unit.quorum {
                continue;
            }

            match unit.leases.iter_mut().find(|(who, _)| who == worker) {
                Some(existing) => existing.1 = now + self.lease,
                None => unit.leases.push((worker.to_owned(), now + self.lease)),
            }
            return Some(Assignment {
                unit: index,
                workload: unit.workload.clone(),
                input: unit.input.clone(),
            });
        }
        None
    }

    /// Take a volunteer's answer and decide what it means.
    ///
    /// # Errors
    ///
    /// See [`Refusal`].
    pub fn submit_result(
        &mut self,
        index: usize,
        submission: Submission,
    ) -> Result<Outcome, Refusal> {
        // Record the answer, and work out what deciding will need — all inside one scope, so
        // the borrow of `units` ends before the workload has to be looked up.
        let pending = {
            let unit = self.units.get_mut(index).ok_or(Refusal::UnknownUnit)?;
            if unit.outcome != Outcome::Open {
                return Err(Refusal::Closed);
            }
            if unit.results.iter().any(|r| r.worker == submission.worker) {
                return Err(Refusal::AlreadyAnswered);
            }
            if !unit.leases.iter().any(|(w, _)| *w == submission.worker) {
                return Err(Refusal::NotLeased);
            }

            unit.leases.retain(|(w, _)| *w != submission.worker);

            // A canary settles here and goes no further. There is nothing to replicate — the
            // coordinator is the second opinion and already has it — and nothing to dispute,
            // because a dispute establishes *which of two claims is true* and this answer is
            // not in doubt. What comes back is evidence about the worker, not about the science.
            if let Some(canary) = unit.canary.clone() {
                let right = submission.output == canary.expected;
                let worker = submission.worker.clone();
                unit.results.push(submission);
                unit.outcome = Outcome::Accepted {
                    output: canary.expected,
                };
                let outcome = unit.outcome.clone();
                if right {
                    self.reputation.passed_a_canary(&worker);
                } else {
                    self.reputation.failed_a_canary(&worker);
                }
                return Ok(outcome);
            }

            unit.results.push(submission);

            if unit.results.len() < unit.quorum {
                return Ok(Outcome::Open);
            }

            let first = unit.results.first().ok_or(Refusal::UnknownUnit)?.clone();
            let second = unit
                .results
                .iter()
                .find(|r| r.output != first.output)
                .cloned();
            (unit.workload.clone(), unit.input.clone(), first, second)
        };

        let (workload_id, input, first, second) = pending;

        let outcome = match second {
            // Everyone who answered agrees. This is what happens to almost every unit, and it
            // is the whole point: accepted after a single execution.
            None => Outcome::Accepted {
                output: first.output,
            },
            Some(second) => {
                let disputable = Arc::clone(
                    &self
                        .workloads
                        .get(&workload_id)
                        .ok_or(Refusal::UnknownUnit)?
                        .disputable,
                );
                self.disagree(index, &workload_id, disputable, input, first, second)
            }
        };

        let unit = self.units.get_mut(index).ok_or(Refusal::UnknownUnit)?;
        unit.outcome = outcome.clone();

        // Work done, credited to everybody who did it. Deliberately not trust earned: almost
        // every unit is accepted after a single execution, so this counts contribution and says
        // nothing about honesty. Only a canary does that.
        if matches!(outcome, Outcome::Accepted { .. }) {
            let workers: Vec<String> = unit.results.iter().map(|r| r.worker.clone()).collect();
            for worker in workers {
                self.reputation.accepted(&worker);
            }
        }
        Ok(outcome)
    }

    /// Two volunteers returned different answers. Choose how to settle it.
    ///
    /// # The one decision in this file that is not bookkeeping
    ///
    /// Bisection is an *interactive* protocol: it works by asking the two parties what state
    /// they claim at a step, and they answer by replaying on their own machines. A party that
    /// cannot replay under Cairn's interpreter cannot answer, and a browser volunteer cannot —
    /// [ADR-0005] is the finding that no host engine will ever expose the state a root covers.
    ///
    /// Challenging such a party anyway would time it out and convict it by default. So the
    /// interactive route is taken only when **both** parties declared they can argue, and
    /// otherwise the referee does the work itself. The fallback is not a stopgap; it is what
    /// honours a volunteer that is fast and blind, which is the volunteer this project is for.
    ///
    /// [ADR-0005]: ../../docs/adr/0005-the-fast-path-cannot-snapshot.md
    fn disagree(
        &mut self,
        unit: usize,
        workload: &str,
        disputable: Arc<Vec<u8>>,
        input: Vec<u8>,
        first: Submission,
        second: Submission,
    ) -> Outcome {
        if first.bisects && second.bisects {
            let index = self.disputes.len();
            self.disputes.push(dispute::open(
                unit,
                workload.to_owned(),
                [first.worker, second.worker],
                [first.output, second.output],
                disputable,
                input,
                self.patience,
            ));
            return Outcome::Disputed { dispute: index };
        }

        let Ok(image) = image::decode(&disputable) else {
            return Outcome::Settled {
                verdict: dispute::ReExecution::NoAnswer,
                refuted: Vec::new(),
                output: None,
            };
        };
        let names = [first.worker, second.worker];
        let (verdict, output) =
            dispute::by_re_execution(&image, &input, &[first.output, second.output]);

        // The referee has just executed the unit and knows which of these two volunteers returned
        // something else. Before ADR-0017 that finding reached the unit's outcome and stopped
        // there, so replication caught cheats and reputation never heard about it.
        //
        // Charged as a wrong answer, not as a lie. The referee proved the *result* wrong and
        // nothing about why: this route exists precisely because these parties cannot argue, and
        // a browser volunteer whose engine diverges gets here in good faith.
        let refuted: Vec<String> = verdict
            .refuted()
            .iter()
            .filter_map(|party| {
                names
                    .get(match party {
                        Party::First => 0,
                        Party::Second => 1,
                    })
                    .cloned()
            })
            .collect();
        for worker in &refuted {
            self.reputation.refuted(worker);
        }

        // And the other one did the work, so it is credited with having done it — the same
        // `accepted` an undisputed unit earns everybody who ran it, and for the same reason:
        // contribution, not trust. Only a canary or a refutation says anything about honesty.
        //
        // Without this a volunteer that computed correctly and had the referee confirm it came
        // out of the disagreement with nothing recorded at all, purely because the *other* party
        // was wrong. Like every other `accepted`, the count does not survive a restart.
        if let ReExecution::Refuted { wrong } = verdict {
            let right = match wrong {
                Party::First => names.get(1),
                Party::Second => names.first(),
            };
            if let Some(worker) = right {
                self.reputation.accepted(worker);
            }
        }

        Outcome::Settled {
            verdict,
            refuted,
            output,
        }
    }
}

/// A per-process value a worker cannot guess, for choosing which units are canaries.
///
/// Not `rand`: a dependency has to do something the standard library cannot, and this needs
/// unpredictability to a volunteer rather than cryptographic randomness. The process id and the
/// clock, hashed, are unpredictable to somebody on the other end of an HTTP connection — and if
/// they are not, the attacker is already inside the machine that decides who cheated.
fn fresh_secret() -> [u8; 32] {
    let mut seed = Vec::with_capacity(24);
    seed.extend_from_slice(&std::process::id().to_le_bytes());
    if let Ok(since) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        seed.extend_from_slice(&since.as_nanos().to_le_bytes());
    }
    *blake3::hash(&seed).as_bytes()
}

/// What came back from a journal, for the line a restarted coordinator prints.
///
/// Counts rather than a log: the interesting number is how much was *not* lost, and the one
/// detail worth naming individually is which arguments were dropped, because somebody was in
/// the middle of them.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Restored {
    /// Workloads registered again.
    pub workloads: usize,
    /// Units queued again.
    pub units: usize,
    /// Results put back.
    pub results: usize,
    /// Units whose outcome was already decided.
    pub decided: usize,
    /// Leases put back as evidence, so a volunteer that was mid-unit is still recognised.
    pub leases: usize,
    /// Canary outcomes put back, so volunteers keep the standing they earned.
    pub canaries: usize,
    /// Units that were mid-argument, and who was arguing. Each is back to `Open`.
    pub voided: Vec<(usize, [String; 2])>,
}

/// What a volunteer is told to do.
#[derive(Clone, Debug)]
pub struct Assignment {
    /// Index to quote back when returning a result.
    pub unit: usize,
    /// Which module to fetch and run.
    pub workload: UnitId,
    /// The input to run it on.
    pub input: Vec<u8>,
}
