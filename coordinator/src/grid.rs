//! What the coordinator knows and what it decides, with no HTTP in it.
//!
//! Every decision Cairn's coordinator makes lives here: which unit to hand a volunteer, what
//! to do with a result that comes back, and when two results mean a dispute rather than a
//! coincidence. The HTTP layer in [`crate::api`] does nothing but translate.
//!
//! The split is not tidiness. A coordinator is the one component that can convict an honest
//! volunteer, so its decisions have to be testable without a socket — and everything in this
//! file is reachable from a unit test that never opens one.
//!
//! # What this is not
//!
//! **There is no database.** State is in memory and dies with the process. That is honest
//! rather than temporary: persistence, transactions and crash recovery are the reason
//! [ADR-0002](../../docs/adr/0002-language-boundaries.md) wanted Spring in the first place,
//! and [ADR-0010](../../docs/adr/0010-the-referee-executes-so-the-coordinator-is-rust.md)
//! explains why that is a documented future rather than a documented plan.
//!
//! **There is no reputation and there are no canaries.** ADR-0001's cost model assumes both —
//! a canary rate `c` and a replication rate `r` — and only `r` exists here. Reputation is what
//! decides *who* gets replicated, and inventing a scoring rule with no real workers to score
//! would be fiction. The replication rate is a dial with a number in it; the rest is not.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use cairn_runtime::canon::{self, Config};
use cairn_runtime::dispute::Party;
use cairn_runtime::engine::image;
use cairn_runtime::engine::machine::{Limits, Machine};
use cairn_runtime::validate;

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
    pub disputable: Vec<u8>,
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
    /// Outstanding leases: worker, and when the lease expires.
    leases: Vec<(String, Instant)>,
    /// Where this unit has got to.
    pub outcome: Outcome,
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
    /// Two volunteers disagreed and the referee decided between them.
    Settled {
        /// The instruction where their executions first differed.
        divergence: u64,
        /// Messages the bisection took.
        rounds: u32,
        /// What the referee concluded, in words, including how it concluded it.
        verdict: String,
        /// The answer, when the referee could establish one.
        output: Option<Vec<u8>>,
    },
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
    replication_percent: u32,
    lease: Duration,
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
            replication_percent: DEFAULT_REPLICATION_PERCENT,
            lease: DEFAULT_LEASE,
        }
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

        // What both parties re-run if somebody disputes: everything.
        let disputable = canon::instrument(&source, Config::dispute_path())
            .map_err(|e| format!("could not instrument for dispute: {e}"))?;

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
        });
        Ok(self.units.len() - 1)
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

    /// Hand a volunteer something to do, or `None` if there is nothing.
    ///
    /// Skips units this worker has already answered — one volunteer, one vote, and a unit
    /// replicated back to the same machine is not replicated at all. That is the single most
    /// important line in this function: it is what makes a quorum mean two *independent*
    /// executions rather than the same execution counted twice.
    pub fn lease(&mut self, worker: &str, now: Instant) -> Option<Assignment> {
        for (index, unit) in self.units.iter_mut().enumerate() {
            if unit.outcome != Outcome::Open {
                continue;
            }
            if unit.results.iter().any(|r| r.worker == worker) {
                continue;
            }
            unit.leases.retain(|(_, expiry)| *expiry > now);
            if unit.leases.iter().any(|(w, _)| w == worker) {
                continue;
            }
            // Outstanding leases plus results already in hand cover the quorum, so nobody else
            // is needed yet.
            if unit.leases.len() + unit.results.len() >= unit.quorum {
                continue;
            }

            unit.leases.push((worker.to_owned(), now + self.lease));
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
                let workload = self
                    .workloads
                    .get(&workload_id)
                    .ok_or(Refusal::UnknownUnit)?;
                settle(&workload.disputable, &input, &first, &second)
            }
        };

        let unit = self.units.get_mut(index).ok_or(Refusal::UnknownUnit)?;
        unit.outcome = outcome.clone();
        Ok(outcome)
    }
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

/// Settle a disagreement.
///
/// # Read this before quoting anything about how disputes are resolved here
///
/// **This is the fallback, not the mechanism.** The referee executes the unit once itself and
/// names whichever party disagrees with what it got. That is ordinary replication, done by the
/// coordinator instead of by a third volunteer, and it costs a full execution.
///
/// The mechanism — bisect to one instruction and execute *that* — is built, tested and
/// demonstrable (`cargo run --example dispute`), and it is **not reachable from here**, for a
/// reason worth being exact about rather than papering over:
///
/// > Bisection is an *interactive* protocol. It works by asking **the two parties** what state
/// > they claim at a given step, and they answer by re-executing on their own machines. The
/// > coordinator never re-executes anything, which is the entire point. There is no wire
/// > protocol for asking yet, so there is nobody to ask.
///
/// Replaying both sides locally and calling the result an arbitration would be worse than this
/// fallback: it would look like the mechanism working while actually being the coordinator
/// doing both parties' work — the exact cost the design exists to avoid, dressed up as its
/// avoidance. So it does the honest cheap thing and says which one it did.
///
/// **What it takes to close the gap** is a question/answer channel: an endpoint where a worker
/// holding an open dispute is asked for `root_at(step)` and answers from a `Replay`. That is an
/// `impl Claimant` over HTTP and a polling loop in the worker. `dispute::resolve` is already
/// generic over `Claimant`, so nothing in `runtime/` changes.
///
/// Until then, this fallback is honest and it is also *correct* — the party disagreeing with a
/// faithful execution of the assigned unit is wrong. What it is not is cheap, and its cost
/// grows with the execution, which is precisely what bisection fixes.
fn settle(disputable: &[u8], input: &[u8], first: &Submission, second: &Submission) -> Outcome {
    let Ok(image) = image::decode(disputable) else {
        return Outcome::Settled {
            divergence: 0,
            rounds: 0,
            verdict: "the disputed module could not be decoded".to_owned(),
            output: None,
        };
    };

    // The coordinator holds the unit's input as assigned, so it knows what the answer was
    // supposed to be. Judging against anything else produces a well-formed verdict for a
    // different question, which is a mistake worth remembering because it looks like an answer.
    let Some(truth) = honest_output(&image, input) else {
        return Outcome::Settled {
            divergence: 0,
            rounds: 0,
            verdict: "the unit could not be executed for adjudication".to_owned(),
            output: None,
        };
    };

    let verdict = match (first.output == truth, second.output == truth) {
        (true, false) => format!("the {} was wrong", Party::Second),
        (false, true) => format!("the {} was wrong", Party::First),
        (false, false) => "neither party's result matches the unit as assigned".to_owned(),
        // Unreachable: they were only routed here because their outputs differ, so they cannot
        // both equal the same bytes. Kept because a silent misbehaviour here would decide a
        // dispute rather than fail.
        (true, true) => {
            "inconsistent — both results match, so this was not a disagreement".to_owned()
        }
    };

    Outcome::Settled {
        divergence: 0,
        rounds: 0,
        verdict: format!("{verdict} (settled by re-execution; see `settle` in grid.rs)"),
        output: Some(truth),
    }
}

/// Execute the unit as assigned, for the answer the referee is prepared to stand behind.
fn honest_output(image: &image::Image<'_>, input: &[u8]) -> Option<Vec<u8>> {
    let mut machine = Machine::new(image, input.to_vec(), Limits::default()).ok()?;
    machine.run().ok().map(|trace| trace.output)
}
