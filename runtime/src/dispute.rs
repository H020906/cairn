//! Settling a disagreement without recomputing the work.
//!
//! Two volunteers return different answers for the same work unit. One of them is wrong, and
//! the coordinator has to find out which — cheaply, because the whole point of Cairn is not
//! paying for the work twice.
//!
//! The parties binary-search their own executions. Each round the coordinator names a step and
//! both must commit to the state they claim to have been in there. Agreement moves the lower
//! bound up; disagreement moves the upper bound down. After `log2(n)` rounds the bounds are
//! adjacent, and the instruction between them is the first on which the two executions
//! differ. The coordinator then re-executes that single instruction and learns who lied.
//!
//! The asymmetry is the point. The parties do the replaying, which is fair — they have a stake
//! in the outcome. The coordinator exchanges `O(log n)` messages and executes one instruction,
//! whether the disputed unit ran for a thousand instructions or a trillion.
//!
//! # Steps, not fuel
//!
//! Positions are step indices: a step is one instruction, so step *n* names exactly one state.
//! Fuel cannot be used for this — it is charged per basic block, so many distinct states share
//! a fuel value and "the state at fuel F" does not identify anything. See
//! [`crate::engine::machine::Snapshot`].
//!
//! # What this module does and does not do
//!
//! It finds *where* the executions diverged. It does not decide *who was right*: that is
//! adjudication, and it needs the pre-state itself rather than a hash of it. [`Verdict`]
//! carries exactly what an adjudicator needs — the agreed root before the instruction, and
//! each party's claim about the root after it.

use crate::engine::image::Image;
use crate::engine::machine::{Limits, Machine, Progress, Witness, WitnessError};
use crate::engine::numeric::Trap;
use crate::merkle::Hash;

/// Rounds after which a challenge is abandoned as malformed.
///
/// Bisection over a `u64` range converges in at most 64 rounds. Exceeding that means the state
/// machine failed to make progress, which is a bug rather than a party's fault.
pub const MAX_ROUNDS: u32 = 64;

/// A position in an execution, counted in instructions executed.
///
/// `Step(0)` is the state before anything ran. `Step(n)` is the state after `n` instructions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Step(u64);

impl Step {
    /// The state before execution begins.
    pub const ZERO: Self = Self(0);

    /// Construct a position from an instruction count.
    #[must_use]
    pub const fn new(instructions: u64) -> Self {
        Self(instructions)
    }

    /// The instruction count.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl core::fmt::Display for Step {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "step {}", self.0)
    }
}

/// Which side of a dispute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Party {
    /// The party whose trace was presented first.
    First,
    /// The other one.
    Second,
}

impl core::fmt::Display for Party {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::First => write!(f, "first party"),
            Self::Second => write!(f, "second party"),
        }
    }
}

/// A party did not answer within its window.
///
/// Volunteers disconnect; this is the ordinary case, not the adversarial one. A party that
/// goes quiet loses by default, but the reputation penalty for silence is materially smaller
/// than for a proven false state transition — see ADR-0001.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Absent;

/// A party to a dispute, able to say what state it claims to have been in.
///
/// Implementations replay their own execution to answer. The trait exists so the protocol can
/// be tested against recorded sequences with no interpreter involved, and driven against real
/// executions by the same code.
pub trait Claimant {
    /// The state root after `step` instructions.
    ///
    /// `Ok(None)` means this party's execution ended before reaching that step — it finished
    /// or trapped. That is a legitimate answer, and two parties that stop at different points
    /// disagree from the first step past the earlier stop.
    ///
    /// # Errors
    ///
    /// [`Absent`] if the party failed to answer.
    fn root_at(&mut self, step: Step) -> Result<Option<Hash>, Absent>;
}

/// What the coordinator should do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "a challenge makes no progress unless the round is played"]
pub enum Round {
    /// Ask both parties to commit to their state at this step.
    Ask {
        /// The step in question.
        step: Step,
    },
    /// The bounds are adjacent; the search is over.
    Settled {
        /// Executing the instruction at this step is what first made the two disagree.
        divergence: Step,
    },
}

/// The interactive bisection, as a state machine.
///
/// Pure: it holds two bounds and a round count, and knows nothing about networks, timeouts or
/// interpreters. A coordinator drives it by playing [`Round::Ask`] against both parties and
/// feeding the answers back through [`Challenge::record`].
///
/// # Invariant
///
/// The parties agree at `agreed` and disagree at `disagreed`, and `agreed < disagreed`. Each
/// round halves the gap while preserving both halves of that, so the search cannot fail to
/// terminate on a genuine disagreement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Challenge {
    agreed: u64,
    disagreed: u64,
    rounds: u32,
}

impl Challenge {
    /// Open a challenge over `[0, length]`.
    ///
    /// The caller must already have established that the parties agree at step 0 and disagree
    /// at `length`; [`resolve`] does that. Returns `None` for a zero length, where there is no
    /// instruction to blame.
    #[must_use]
    pub const fn open(length: Step) -> Option<Self> {
        if length.0 == 0 {
            return None;
        }
        Some(Self {
            agreed: 0,
            disagreed: length.0,
            rounds: 0,
        })
    }

    /// The step to put to both parties, or the settled answer.
    pub const fn round(&self) -> Round {
        if self.disagreed == self.agreed + 1 {
            return Round::Settled {
                divergence: Step(self.agreed),
            };
        }
        // Halving the gap rather than averaging the bounds keeps the arithmetic away from
        // overflow at the top of the range.
        Round::Ask {
            step: Step(self.agreed + (self.disagreed - self.agreed) / 2),
        }
    }

    /// Record what the two parties claimed at the step [`Challenge::round`] named.
    ///
    /// Agreement moves the lower bound up, disagreement moves the upper bound down. Calling
    /// this on a settled challenge does nothing.
    pub fn record(&mut self, first: Option<Hash>, second: Option<Hash>) {
        let Round::Ask { step } = self.round() else {
            return;
        };
        self.rounds = self.rounds.saturating_add(1);
        if first == second {
            self.agreed = step.0;
        } else {
            self.disagreed = step.0;
        }
    }

    /// Messages exchanged so far.
    #[must_use]
    pub const fn rounds(&self) -> u32 {
        self.rounds
    }

    /// The current bracket: the parties agree at the first and disagree at the second.
    #[must_use]
    pub const fn bounds(&self) -> (Step, Step) {
        (Step(self.agreed), Step(self.disagreed))
    }
}

/// Everything an adjudicator needs to decide who lied.
///
/// The two parties agreed on the state entering the instruction and disagree on the state
/// leaving it. Executing that one instruction from the agreed state produces a root matching
/// exactly one of the two claims — or neither, if both parties are wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    /// Executing the instruction at this step is what first made the two disagree.
    pub divergence: Step,
    /// The root both parties agreed on immediately before it.
    pub agreed_root: Option<Hash>,
    /// What the first party claims the state became.
    pub first_claim: Option<Hash>,
    /// What the second party claims the state became.
    pub second_claim: Option<Hash>,
    /// Messages exchanged to get here.
    pub rounds: u32,
}

/// Why a challenge could not be settled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisputeError {
    /// The parties agree at the end, so there was nothing to settle.
    ///
    /// Not a failure of the protocol — a challenge should not have been opened.
    NoDisagreement,
    /// The parties disagree before executing anything.
    ///
    /// They are not running the same work unit, or one has corrupted its inputs. Bisection
    /// cannot attribute that to an instruction, because no instruction has run.
    DisagreeAtStart,
    /// Nothing executed, so no instruction can be blamed.
    EmptyExecution,
    /// A party stopped answering.
    Abandoned {
        /// The party that went quiet.
        by: Party,
        /// The step it was asked about.
        at: Step,
        /// Rounds completed before it did.
        rounds: u32,
    },
    /// The search failed to converge within [`MAX_ROUNDS`], which means a bug here rather
    /// than misbehaviour by a party.
    DidNotConverge,
}

impl core::fmt::Display for DisputeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoDisagreement => write!(f, "the parties agree; there is no dispute to settle"),
            Self::DisagreeAtStart => write!(
                f,
                "the parties disagree before executing anything, so they are not running the \
                 same work unit"
            ),
            Self::EmptyExecution => write!(f, "nothing executed, so no instruction can be blamed"),
            Self::Abandoned { by, at, rounds } => {
                write!(
                    f,
                    "the {by} stopped answering at {at} after {rounds} rounds"
                )
            }
            Self::DidNotConverge => {
                write!(f, "bisection did not converge within {MAX_ROUNDS} rounds")
            }
        }
    }
}

impl std::error::Error for DisputeError {}

/// Ask one party, attributing silence to it.
fn ask(
    claimant: &mut impl Claimant,
    party: Party,
    step: Step,
    rounds: u32,
) -> Result<Option<Hash>, DisputeError> {
    claimant
        .root_at(step)
        .map_err(|Absent| DisputeError::Abandoned {
            by: party,
            at: step,
            rounds,
        })
}

/// Run the bisection to completion against two parties.
///
/// `length` should be the longer of the two executions, so that a party which stopped early
/// shows up as disagreeing from the first step past its end.
///
/// # Errors
///
/// See [`DisputeError`]. Note that [`DisputeError::NoDisagreement`] means the challenge should
/// not have been opened rather than that anything went wrong.
pub fn resolve(
    first: &mut impl Claimant,
    second: &mut impl Claimant,
    length: Step,
) -> Result<Verdict, DisputeError> {
    let Some(mut challenge) = Challenge::open(length) else {
        return Err(DisputeError::EmptyExecution);
    };

    // Both ends must hold before the search means anything: the parties start from the same
    // state and end in different ones.
    let start_first = ask(first, Party::First, Step::ZERO, 0)?;
    let start_second = ask(second, Party::Second, Step::ZERO, 0)?;
    if start_first != start_second {
        return Err(DisputeError::DisagreeAtStart);
    }

    let end_first = ask(first, Party::First, length, 0)?;
    let end_second = ask(second, Party::Second, length, 0)?;
    if end_first == end_second {
        return Err(DisputeError::NoDisagreement);
    }

    loop {
        match challenge.round() {
            Round::Settled { divergence } => {
                let after = Step(divergence.0 + 1);
                return Ok(Verdict {
                    divergence,
                    agreed_root: ask(first, Party::First, divergence, challenge.rounds)?,
                    first_claim: ask(first, Party::First, after, challenge.rounds)?,
                    second_claim: ask(second, Party::Second, after, challenge.rounds)?,
                    rounds: challenge.rounds,
                });
            }
            Round::Ask { step } => {
                if challenge.rounds >= MAX_ROUNDS {
                    return Err(DisputeError::DidNotConverge);
                }
                let a = ask(first, Party::First, step, challenge.rounds)?;
                let b = ask(second, Party::Second, step, challenge.rounds)?;
                challenge.record(a, b);
            }
        }
    }
}

/// Who the adjudicator found to be telling the truth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Judgment {
    /// One party's claim matched what the instruction actually produces.
    Guilty {
        /// The party whose claim was wrong.
        liar: Party,
    },
    /// Neither claim matched. Both parties are wrong, which is not a case the protocol can
    /// resolve in anyone's favour — the unit goes back to the queue and both take a penalty.
    BothWrong {
        /// What executing the instruction actually produces, or `None` if it trapped and
        /// there is no state after it. Not a hash, because there is none to report.
        actual: Option<Hash>,
    },
    /// Both claims matched, which cannot happen: bisection only converges where they differ.
    /// Reaching this means the verdict and the witness describe different disputes.
    Inconsistent,
}

/// Why a disputed instruction could not be adjudicated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdjudicationError {
    /// The witness does not describe the state the parties agreed on.
    ///
    /// Its reconstructed commitment differs from [`Verdict::agreed_root`], so whoever supplied
    /// it fabricated or corrupted it.
    WitnessDoesNotMatchAgreedState,
    /// The witness could not be turned back into a machine.
    UnusableWitness(WitnessError),
    /// The disputed instruction could not be executed from the witness.
    ///
    /// A [`Trap::WitnessIncomplete`] here means the witness omitted a page the instruction
    /// needs, which is the supplier's failure rather than a fact about the computation. Any
    /// other trap is a real outcome and is compared like any other result.
    ExecutionFailed(Trap),
    /// Bisection never produced an agreed root to start from.
    NoAgreedState,
}

impl core::fmt::Display for AdjudicationError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::WitnessDoesNotMatchAgreedState => write!(
                f,
                "the witness does not reconstruct the state the parties agreed on"
            ),
            Self::UnusableWitness(e) => write!(f, "unusable witness: {e}"),
            Self::ExecutionFailed(t) => {
                write!(f, "could not execute the disputed instruction: {t}")
            }
            Self::NoAgreedState => write!(f, "bisection produced no agreed state to execute from"),
        }
    }
}

impl std::error::Error for AdjudicationError {}

/// Execute the disputed instruction and decide who lied.
///
/// # How this stays cheap
///
/// The adjudicator never replays the execution. It is handed the agreed state as a
/// [`Witness`] — small state whole, memory as the handful of pages the instruction touches
/// with proofs — verifies it against the root bisection already established, executes exactly
/// one instruction, and compares.
///
/// The cost therefore depends on the *module's* declared stack depth and on how many pages one
/// instruction can touch, and **not on the length of the disputed execution**. Arbitrating a
/// unit that ran for a trillion instructions costs what arbitrating one that ran for a
/// thousand costs. That is the property ADR-0001 rests on.
///
/// # Errors
///
/// See [`AdjudicationError`]. Note that a trap during the disputed instruction is *not* an
/// error unless it is [`Trap::WitnessIncomplete`]: a trapping instruction is a legitimate
/// outcome that a party may have claimed correctly or incorrectly.
pub fn adjudicate(
    image: &Image<'_>,
    verdict: &Verdict,
    witness: &Witness,
    input: &[u8],
    limits: Limits,
) -> Result<Judgment, AdjudicationError> {
    let Some(agreed) = verdict.agreed_root else {
        return Err(AdjudicationError::NoAgreedState);
    };

    // The single check that makes the witness trustworthy. The commitment covers memory,
    // globals, both stacks, the program counter and the fuel, so a witness reproducing this
    // root is the agreed state in every respect that matters.
    if witness.commitment().root() != agreed {
        return Err(AdjudicationError::WitnessDoesNotMatchAgreedState);
    }

    let mut machine = Machine::restore(image, witness, input.to_vec(), limits)
        .map_err(AdjudicationError::UnusableWitness)?;

    let actual = match machine.step() {
        Ok(_) => Some(machine.commit().root()),
        // A witness that omits a page the instruction needs is unusable; the supplier failed.
        Err(trap @ Trap::WitnessIncomplete { .. }) => {
            return Err(AdjudicationError::ExecutionFailed(trap))
        }
        // Any other trap is a real outcome: execution stops, and a party claiming a state
        // after it is wrong.
        Err(_) => None,
    };

    Ok(
        match (
            actual == verdict.first_claim,
            actual == verdict.second_claim,
        ) {
            (true, false) => Judgment::Guilty {
                liar: Party::Second,
            },
            (false, true) => Judgment::Guilty { liar: Party::First },
            (false, false) => Judgment::BothWrong { actual },
            (true, true) => Judgment::Inconsistent,
        },
    )
}

/// How many resume points [`Replay`] keeps by default.
///
/// The whole trade is here. Each checkpoint is a full machine state — most of it linear
/// memory — so the memory cost is roughly `budget × the workload's memory ceiling`, while the
/// time cost of an answer falls as `n / budget`. Thirty-two keeps a bisection to under two
/// executions' worth of stepping, and costs a few tens of megabytes on a workload using a
/// megabyte of memory.
///
/// A worker with less memory should lower it rather than accept swapping; a native worker with
/// plenty can raise it. Nothing the coordinator sees changes either way.
pub const DEFAULT_CHECKPOINT_BUDGET: usize = 32;

/// Never checkpoint more often than this, whatever the budget allows.
///
/// A checkpoint copies the whole machine, most of it linear memory. Below a few thousand
/// instructions, replaying from further back is cheaper than the copy that would avoid it —
/// measured, an unbounded interval made the shortest disputes about 30% *slower*. This is where
/// the two curves cross on the workloads in `benches/cost.rs`; it is a constant rather than
/// something adaptive because getting it roughly right is enough.
const MIN_CHECKPOINT_INTERVAL: u64 = 4096;

/// A party that answers by re-running its own execution.
///
/// # Cost, and why this keeps checkpoints
///
/// A bisection asks this party `log₂(n)` questions, each naming a step. Answering one by
/// replaying from step 0 costs `O(n)`, so a naive party pays `O(n log n)` for a dispute — on
/// top of the fact that it must answer with **Cairn's interpreter**, since a trace commitment
/// covers machine state no host engine exposes ([ADR-0005]). The interpreter measures 37×–142×
/// slower than the JIT the work was originally done on, so the multiplier on `n` is already
/// large before `log n` is applied to it.
///
/// So this keeps periodic **full machine states** — not the roots the trace commits to, the
/// actual states — and resumes from the nearest one at or below the requested step. That makes
/// an answer cost at most one checkpoint interval and a dispute `O(n)`, which
/// [ADR-0008](../../docs/adr/0008-a-dispute-costs-an-interpreted-re-execution.md) puts at about
/// half of what a dispute costs a party.
///
/// # Why the states are recorded lazily
///
/// The obvious implementation runs the execution once up front to lay checkpoints down evenly,
/// then answers from them. Measured, that made short disputes **slower**, and the reason is
/// worth keeping: how much a naive replay costs depends on *where the parties diverged*, not on
/// how long the execution was. A dispute that diverges in its first few instructions is
/// answered by replaying a few instructions, `log₂(n)` times — already cheap — and a
/// preparatory sweep charges it a full execution for nothing.
///
/// So checkpoints are recorded while answering, never ahead of it. This party never steps an
/// instruction it would not have stepped anyway, which makes checkpointing a pure improvement
/// instead of a bet on where the divergence is.
///
/// # Why this is safe to vary
///
/// Checkpointing is **invisible to the protocol**. It changes when this party does its work,
/// never what it answers, because a resumed machine is a bit-exact copy of one that got there
/// by stepping. Two honest workers with different budgets — or one with checkpointing disabled
/// entirely — return identical roots for identical questions, and there is a test for exactly
/// that. Keep it that way: if anything the coordinator can observe ever depends on the budget,
/// this has become a protocol artefact and honest workers with different memory can be
/// convicted for it.
///
/// [ADR-0005]: ../../docs/adr/0005-the-fast-path-cannot-snapshot.md
pub struct Replay<'a> {
    image: &'a Image<'a>,
    input: Vec<u8>,
    limits: Limits,
    /// Resume points, in increasing step order. Empty until the first answer.
    checkpoints: Vec<(u64, Machine<'a>)>,
    /// Steps between kept checkpoints. Doubles whenever the budget would be exceeded.
    interval: u64,
    /// Maximum resume points to hold. Zero replays from the start every time.
    budget: usize,
}

impl<'a> Replay<'a> {
    /// A party that will replay `image` on `input` to answer, keeping the default budget of
    /// resume points.
    #[must_use]
    pub fn new(image: &'a Image<'a>, input: Vec<u8>, limits: Limits) -> Self {
        Self::with_checkpoint_budget(image, input, limits, DEFAULT_CHECKPOINT_BUDGET)
    }

    /// The same, holding at most `budget` resume points.
    ///
    /// `0` disables checkpointing, which is slower by a factor of `log₂(n)` and answers
    /// identically. Useful for memory-constrained workers, and for proving in tests that the
    /// answers do not depend on this.
    #[must_use]
    pub fn with_checkpoint_budget(
        image: &'a Image<'a>,
        input: Vec<u8>,
        limits: Limits,
        budget: usize,
    ) -> Self {
        Self {
            image,
            input,
            limits,
            checkpoints: Vec::new(),
            interval: 1,
            budget,
        }
    }

    /// Fold newly recorded states into the kept set, thinning if that exceeds the budget.
    ///
    /// Thinning drops every other checkpoint, which roughly doubles the spacing, so the
    /// interval is doubled to match. Uneven spacing is tolerable — a resume point that is
    /// closer than the interval only makes an answer cheaper.
    fn absorb(&mut self, recorded: Vec<(u64, Machine<'a>)>) {
        self.checkpoints.extend(recorded);
        self.checkpoints.sort_by_key(|(at, _)| *at);
        self.checkpoints.dedup_by_key(|(at, _)| *at);

        while self.budget > 0 && self.checkpoints.len() > self.budget {
            let mut keep = false;
            self.checkpoints.retain(|_| {
                keep = !keep;
                keep
            });
            self.interval = self.interval.saturating_mul(2);
        }
    }

    /// The latest recorded state at or before `step`, or a fresh machine.
    fn resume_at(&self, step: u64) -> Option<(u64, Machine<'a>)> {
        let found = self
            .checkpoints
            .iter()
            .rev()
            .find(|(at, _)| *at <= step)
            .map(|(at, machine)| (*at, machine.clone()));
        match found {
            Some(resumed) => Some(resumed),
            None => Machine::new(self.image, self.input.clone(), self.limits)
                .ok()
                .map(|machine| (0, machine)),
        }
    }
}

impl Claimant for Replay<'_> {
    fn root_at(&mut self, step: Step) -> Result<Option<Hash>, Absent> {
        // Spacing is derived from the largest step ever asked for, because this party does not
        // know the execution's length and cannot ask. It only ever grows.
        //
        // Deriving it from the *first* question instead looks equivalent and is a trap: a
        // bisection's opening exchange includes step 0, which would set the interval to 1 and
        // clone the whole machine on every instruction. That is not a slow path, it is a hang.
        if self.budget > 0 {
            let implied = step.get() / self.budget as u64;
            self.interval = self.interval.max(implied).max(MIN_CHECKPOINT_INTERVAL);
        }

        let Some((from, mut machine)) = self.resume_at(step.get()) else {
            return Ok(None);
        };

        // Recorded while replaying rather than in a preparatory pass over the whole execution.
        // An eager sweep costs a full execution before answering anything, and a dispute whose
        // divergence is early never needs it — measured, that made short disputes *slower*.
        // Recording opportunistically means this party never steps an instruction it would not
        // have stepped anyway.
        let mut recorded = Vec::new();
        let mut next_record = if self.budget == 0 {
            u64::MAX
        } else {
            from.saturating_add(self.interval)
        };

        let result = loop {
            if machine.steps() >= step.get() {
                break Ok(Some(machine.commit().root()));
            }
            if machine.steps() >= next_record {
                recorded.push((machine.steps(), machine.clone()));
                next_record = machine.steps().saturating_add(self.interval);
            }
            match machine.step() {
                // Execution ended. If it ended exactly here, this is the final state;
                // otherwise the requested step is past the end.
                Ok(Progress::Finished) => {
                    break Ok((machine.steps() == step.get()).then(|| machine.commit().root()));
                }
                Ok(_) => {}
                // A trapped execution has no state at or after the trap.
                Err(_) => break Ok(None),
            }
        };

        self.absorb(recorded);
        result
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use super::*;

    /// A distinct hash per number, so test sequences are readable.
    fn h(n: u64) -> Hash {
        *blake3::hash(&n.to_le_bytes()).as_bytes()
    }

    /// A party reading from a recorded sequence of states.
    struct Recorded {
        roots: Vec<Option<Hash>>,
        absent_from: Option<u64>,
        queries: u32,
    }

    impl Recorded {
        /// A party whose execution diverges from the common prefix at `divergence`.
        fn diverging_at(length: u64, divergence: u64, tag: u64) -> Self {
            Self {
                roots: (0..=length)
                    .map(|i| {
                        Some(if i < divergence {
                            h(i)
                        } else {
                            h(i * 1000 + tag)
                        })
                    })
                    .collect(),
                absent_from: None,
                queries: 0,
            }
        }

        /// A party that stops answering once asked about `step` or beyond.
        fn absent_from(mut self, step: u64) -> Self {
            self.absent_from = Some(step);
            self
        }
    }

    impl Claimant for Recorded {
        fn root_at(&mut self, step: Step) -> Result<Option<Hash>, Absent> {
            self.queries += 1;
            if self.absent_from.is_some_and(|from| step.get() >= from) {
                return Err(Absent);
            }
            Ok(self.roots.get(step.get() as usize).copied().flatten())
        }
    }

    #[test]
    fn finds_the_first_divergent_instruction_wherever_it_is() {
        // Exhaustive over a small range rather than sampled: every divergence point in a
        // 64-instruction execution is checked, which is a proof for that size rather than
        // evidence.
        const LENGTH: u64 = 64;
        for divergence in 1..=LENGTH {
            let mut a = Recorded::diverging_at(LENGTH, divergence, 1);
            let mut b = Recorded::diverging_at(LENGTH, divergence, 2);
            let verdict = resolve(&mut a, &mut b, Step::new(LENGTH)).expect("should settle");
            assert_eq!(
                verdict.divergence,
                Step::new(divergence - 1),
                "the instruction running from step {} to {divergence} is the culprit",
                divergence - 1
            );
            assert_ne!(verdict.first_claim, verdict.second_claim);
        }
    }

    #[test]
    fn the_verdict_carries_what_an_adjudicator_needs() {
        // The agreed pre-state and the two claimed post-states. An adjudicator executes one
        // instruction from the first and sees which of the other two it produces.
        let mut a = Recorded::diverging_at(32, 10, 1);
        let mut b = Recorded::diverging_at(32, 10, 2);
        let verdict = resolve(&mut a, &mut b, Step::new(32)).unwrap();

        assert_eq!(verdict.divergence, Step::new(9));
        assert_eq!(verdict.agreed_root, Some(h(9)));
        assert_eq!(verdict.first_claim, Some(h(10 * 1000 + 1)));
        assert_eq!(verdict.second_claim, Some(h(10 * 1000 + 2)));
    }

    #[test]
    fn the_round_count_is_logarithmic() {
        // The whole economic argument. A trillion-instruction execution must settle in a few
        // dozen messages, not a trillion.
        for exponent in 1..=20u32 {
            let length = 1u64 << exponent;
            let mut a = Recorded::diverging_at(length, length, 1);
            let mut b = Recorded::diverging_at(length, length, 2);
            let verdict = resolve(&mut a, &mut b, Step::new(length)).unwrap();
            assert!(
                verdict.rounds <= exponent,
                "2^{exponent} instructions took {} rounds",
                verdict.rounds
            );
        }
    }

    #[test]
    fn a_party_answers_a_logarithmic_number_of_queries() {
        // The cost that falls on the parties is bounded too, not just the coordinator's.
        let length = 1u64 << 16;
        let mut a = Recorded::diverging_at(length, 12_345, 1);
        let mut b = Recorded::diverging_at(length, 12_345, 2);
        resolve(&mut a, &mut b, Step::new(length)).unwrap();
        assert!(a.queries < 32, "first party answered {} queries", a.queries);
        assert!(
            b.queries < 32,
            "second party answered {} queries",
            b.queries
        );
    }

    #[test]
    fn agreement_is_not_a_dispute() {
        let mut a = Recorded::diverging_at(32, 99, 1);
        let mut b = Recorded::diverging_at(32, 99, 1);
        assert_eq!(
            resolve(&mut a, &mut b, Step::new(32)).unwrap_err(),
            DisputeError::NoDisagreement
        );
    }

    #[test]
    fn disagreeing_before_anything_runs_is_not_settleable() {
        // No instruction has executed, so none can be blamed. Two parties in this state are
        // not running the same work unit, which is a different problem.
        let mut a = Recorded::diverging_at(32, 0, 1);
        let mut b = Recorded::diverging_at(32, 0, 2);
        assert_eq!(
            resolve(&mut a, &mut b, Step::new(32)).unwrap_err(),
            DisputeError::DisagreeAtStart
        );
    }

    #[test]
    fn an_empty_execution_has_no_instruction_to_blame() {
        assert_eq!(Challenge::open(Step::ZERO), None);
        let mut a = Recorded::diverging_at(0, 0, 1);
        let mut b = Recorded::diverging_at(0, 0, 2);
        assert_eq!(
            resolve(&mut a, &mut b, Step::ZERO).unwrap_err(),
            DisputeError::EmptyExecution
        );
    }

    #[test]
    fn a_party_that_stops_answering_loses_by_default() {
        // Volunteers disconnect. The protocol has to name who went quiet so the coordinator
        // can apply the lighter penalty silence earns, rather than the one a proven lie does.
        let mut a = Recorded::diverging_at(64, 20, 1);
        let mut b = Recorded::diverging_at(64, 20, 2).absent_from(32);

        match resolve(&mut a, &mut b, Step::new(64)).unwrap_err() {
            DisputeError::Abandoned { by, at, .. } => {
                assert_eq!(by, Party::Second);
                assert!(at.get() >= 32);
            }
            other => panic!("expected abandonment, got {other:?}"),
        }
    }

    #[test]
    fn executions_of_different_lengths_diverge_where_the_shorter_one_stopped() {
        // One party finished or trapped at step 40 while the other ran on. They agree
        // everywhere up to 40; the first step past it is where they part.
        let short: Vec<Option<Hash>> = (0..=64).map(|i| (i <= 40).then(|| h(i))).collect();
        let long: Vec<Option<Hash>> = (0..=64).map(|i| Some(h(i))).collect();

        let mut a = Recorded {
            roots: short,
            absent_from: None,
            queries: 0,
        };
        let mut b = Recorded {
            roots: long,
            absent_from: None,
            queries: 0,
        };

        let verdict = resolve(&mut a, &mut b, Step::new(64)).unwrap();
        assert_eq!(verdict.divergence, Step::new(40));
        assert_eq!(
            verdict.first_claim, None,
            "the shorter party has no state there"
        );
        assert_eq!(verdict.second_claim, Some(h(41)));
    }

    #[test]
    fn the_state_machine_preserves_its_invariant() {
        // Whatever answers arrive, the parties always agree at the lower bound and disagree at
        // the upper one, and the gap always shrinks. Driven here with adversarial answers that
        // alternate, which no honest pair would produce.
        let mut challenge = Challenge::open(Step::new(1000)).unwrap();
        let mut alternate = false;
        let mut previous_gap = 1000u64;

        loop {
            let (lo, hi) = challenge.bounds();
            assert!(lo < hi, "bounds crossed: {lo} .. {hi}");
            let gap = hi.get() - lo.get();
            assert!(gap <= previous_gap, "the gap grew");
            previous_gap = gap;

            match challenge.round() {
                Round::Settled { divergence } => {
                    assert_eq!(divergence, lo);
                    assert_eq!(hi.get(), lo.get() + 1);
                    break;
                }
                Round::Ask { step } => {
                    assert!(lo < step && step < hi, "{step} outside {lo}..{hi}");
                    alternate = !alternate;
                    if alternate {
                        challenge.record(Some(h(1)), Some(h(1)));
                    } else {
                        challenge.record(Some(h(1)), Some(h(2)));
                    }
                }
            }
        }
        assert!(
            challenge.rounds() <= 10,
            "1000 steps took {} rounds",
            challenge.rounds()
        );
    }

    #[test]
    fn recording_a_settled_challenge_does_nothing() {
        let mut challenge = Challenge::open(Step::new(1)).unwrap();
        assert!(matches!(challenge.round(), Round::Settled { .. }));
        let before = challenge.clone();
        challenge.record(Some(h(1)), Some(h(2)));
        assert_eq!(challenge, before);
    }

    // --- against real executions -------------------------------------------------------

    /// Assemble, validate and instrument, as a coordinator would.
    fn canonical(text: &str) -> Vec<u8> {
        use crate::canon::{self, Config};
        let source = wat::parse_str(text).expect("module should assemble");
        crate::validate::validate_submitted(&source, crate::validate::Limits::default())
            .expect("module should be a valid Cairn workload");
        canon::instrument(&source, Config::default()).expect("instrumentation should succeed")
    }

    /// Reads its input into memory and writes it back out. Two runs on different inputs share
    /// an identical prefix — the input is supplied by a host call and is not part of the
    /// initial state — and part company at the instruction that copies it into memory.
    const ECHO: &str = r#"
        (module
          (import "cairn" "input"  (func $input  (param i32 i32) (result i32)))
          (import "cairn" "output" (func $output (param i32 i32)))
          (memory (export "memory") 1 4)
          (func (export "cairn_run") (local $len i32)
            (local.set $len (call $input (i32.const 0) (i32.const 0)))
            (drop (call $input (i32.const 64) (local.get $len)))
            (call $output (i32.const 64) (local.get $len))))
    "#;

    #[test]
    fn settles_a_dispute_between_two_real_executions() {
        let module = canonical(ECHO);
        let image = crate::engine::image::decode(&module).unwrap();

        // Establish what each party actually did, so the challenge is opened over the longer
        // of the two the way a coordinator would.
        let run = |input: &[u8]| {
            let mut machine = Machine::new(&image, input.to_vec(), Limits::default()).unwrap();
            machine.run().unwrap()
        };
        let left = run(b"aaaaaaaa");
        let right = run(b"bbbbbbbb");
        assert_ne!(left.final_root, right.final_root, "the runs must differ");

        let length = Step::new(left.steps.max(right.steps));
        let mut first = Replay::new(&image, b"aaaaaaaa".to_vec(), Limits::default());
        let mut second = Replay::new(&image, b"bbbbbbbb".to_vec(), Limits::default());

        let verdict = resolve(&mut first, &mut second, length).expect("should settle");

        // The claim the protocol makes, checked directly against the executions rather than
        // taken on trust: they agree entering the instruction and disagree leaving it.
        assert_eq!(
            first.root_at(verdict.divergence).unwrap(),
            second.root_at(verdict.divergence).unwrap(),
            "the parties must agree at the step the verdict names"
        );
        assert_ne!(
            verdict.first_claim, verdict.second_claim,
            "and disagree one instruction later"
        );
        assert_eq!(
            verdict.agreed_root,
            first.root_at(verdict.divergence).unwrap()
        );
    }

    #[test]
    fn the_divergence_is_the_first_one_not_merely_some_divergence() {
        // Bisection is only useful if it lands on the *first* difference. Verified by walking
        // every earlier step and confirming the two executions match there.
        let module = canonical(ECHO);
        let image = crate::engine::image::decode(&module).unwrap();

        let mut first = Replay::new(&image, b"xyz".to_vec(), Limits::default());
        let mut second = Replay::new(&image, b"abc".to_vec(), Limits::default());

        let mut probe = Machine::new(&image, b"xyz".to_vec(), Limits::default()).unwrap();
        let length = Step::new(probe.run().unwrap().steps);

        let verdict = resolve(&mut first, &mut second, length).unwrap();

        for step in 0..=verdict.divergence.get() {
            let step = Step::new(step);
            assert_eq!(
                first.root_at(step).unwrap(),
                second.root_at(step).unwrap(),
                "the executions already differed at {step}, before the verdict's divergence"
            );
        }
    }

    #[test]
    fn identical_executions_produce_no_dispute() {
        let module = canonical(ECHO);
        let image = crate::engine::image::decode(&module).unwrap();

        let mut probe = Machine::new(&image, b"same".to_vec(), Limits::default()).unwrap();
        let length = Step::new(probe.run().unwrap().steps);

        let mut first = Replay::new(&image, b"same".to_vec(), Limits::default());
        let mut second = Replay::new(&image, b"same".to_vec(), Limits::default());

        assert_eq!(
            resolve(&mut first, &mut second, length).unwrap_err(),
            DisputeError::NoDisagreement
        );
    }

    #[test]
    fn replay_reports_no_state_past_the_end_of_an_execution() {
        let module = canonical(ECHO);
        let image = crate::engine::image::decode(&module).unwrap();

        let mut probe = Machine::new(&image, b"hi".to_vec(), Limits::default()).unwrap();
        let total = probe.run().unwrap().steps;

        let mut party = Replay::new(&image, b"hi".to_vec(), Limits::default());
        assert!(
            party.root_at(Step::new(total)).unwrap().is_some(),
            "the final state is reachable"
        );
        assert_eq!(
            party.root_at(Step::new(total + 1)).unwrap(),
            None,
            "there is nothing past the end"
        );
    }

    #[test]
    fn replay_agrees_with_a_straight_run() {
        // The replay path and the run path must produce the same roots, or a party would
        // appear to contradict its own submitted trace.
        let module = canonical(ECHO);
        let image = crate::engine::image::decode(&module).unwrap();

        let mut machine = Machine::new(&image, b"check".to_vec(), Limits::default()).unwrap();
        let trace = machine.run().unwrap();

        let mut party = Replay::new(&image, b"check".to_vec(), Limits::default());
        assert_eq!(party.root_at(Step::ZERO).unwrap(), Some(trace.initial));
        assert_eq!(
            party.root_at(Step::new(trace.steps)).unwrap(),
            Some(trace.final_root)
        );
        for snapshot in &trace.snapshots {
            assert_eq!(
                party.root_at(Step::new(snapshot.step)).unwrap(),
                Some(snapshot.root),
                "replay disagreed with the committed snapshot at step {}",
                snapshot.step
            );
        }
    }

    #[test]
    fn checkpointing_changes_speed_and_nothing_else() {
        // The property that makes checkpointing safe. A party's answers must not depend on how
        // much memory it chose to spend, or two honest workers with different budgets would
        // give different roots for the same question and the protocol would convict one of
        // them. Budget 0 replays from step 0 every time; the others resume from a checkpoint,
        // and 1 forces the halve-and-double path to run repeatedly.
        let module = canonical(ECHO);
        let image = crate::engine::image::decode(&module).unwrap();

        let mut probe = Machine::new(&image, b"budget".to_vec(), Limits::default()).unwrap();
        let total = probe.run().unwrap().steps;

        let mut reference =
            Replay::with_checkpoint_budget(&image, b"budget".to_vec(), Limits::default(), 0);
        let expected: Vec<_> = (0..=total + 2)
            .map(|step| reference.root_at(Step::new(step)).unwrap())
            .collect();

        for budget in [1usize, 2, 3, 7, 32, 1024] {
            let mut party = Replay::with_checkpoint_budget(
                &image,
                b"budget".to_vec(),
                Limits::default(),
                budget,
            );
            for (step, want) in expected.iter().enumerate() {
                assert_eq!(
                    party.root_at(Step::new(step as u64)).unwrap(),
                    *want,
                    "budget {budget} answered differently at step {step}"
                );
            }
        }
    }

    #[test]
    fn checkpoints_stay_within_their_budget() {
        // Otherwise a long execution would hold one full machine state per interval and a
        // worker would run out of memory rather than run slowly.
        let module = canonical(ECHO);
        let image = crate::engine::image::decode(&module).unwrap();

        for budget in [1usize, 2, 5, 16] {
            let mut party =
                Replay::with_checkpoint_budget(&image, b"hold".to_vec(), Limits::default(), budget);
            let _ = party.root_at(Step::new(1)).unwrap();
            assert!(
                party.checkpoints.len() <= budget,
                "budget {budget} kept {} checkpoints",
                party.checkpoints.len()
            );
        }
    }

    #[test]
    fn a_resumed_answer_costs_less_than_replaying_from_zero() {
        // The point of the exercise. Counted in instructions stepped rather than in wall-clock,
        // so it says something reproducible about the algorithm rather than about this machine.
        let module = canonical(ECHO);
        let image = crate::engine::image::decode(&module).unwrap();

        let mut probe = Machine::new(&image, b"cost".to_vec(), Limits::default()).unwrap();
        let total = probe.run().unwrap().steps;
        assert!(total > 8, "the fixture needs enough steps to bisect over");

        let mut party =
            Replay::with_checkpoint_budget(&image, b"cost".to_vec(), Limits::default(), 8);
        let _ = party.root_at(Step::new(total)).unwrap();

        // Every answer starts from a checkpoint no further back than one interval.
        let interval = party.interval;
        for step in 0..=total {
            let (from, _) = party.resume_at(step).unwrap();
            assert!(
                step - from < interval.max(1) * 2,
                "answering step {step} would replay from {from}, more than an interval back"
            );
        }
    }

    #[test]
    fn a_query_at_step_zero_does_not_collapse_the_interval() {
        // A bisection's opening exchange asks about step 0. An earlier version derived the
        // checkpoint interval from the first question it saw, which made that interval 1 and
        // cloned the entire machine on every instruction of every later answer. The benchmark
        // stopped finishing.
        let module = canonical(ECHO);
        let image = crate::engine::image::decode(&module).unwrap();

        let mut probe = Machine::new(&image, b"zero".to_vec(), Limits::default()).unwrap();
        let total = probe.run().unwrap().steps;

        let mut party =
            Replay::with_checkpoint_budget(&image, b"zero".to_vec(), Limits::default(), 4);
        let _ = party.root_at(Step::ZERO).unwrap();
        let _ = party.root_at(Step::new(total)).unwrap();

        assert!(
            party.interval > 1,
            "interval collapsed to {} after a query at step 0",
            party.interval
        );
        assert!(party.checkpoints.len() <= 4);
    }

    // --- adjudication ------------------------------------------------------------------

    /// Run a machine to `step` and capture a witness for the instruction that follows.
    fn witness_at(
        image: &crate::engine::image::Image<'_>,
        input: &[u8],
        step: Step,
    ) -> crate::engine::machine::Witness {
        let mut machine = Machine::new(image, input.to_vec(), Limits::default()).unwrap();
        for _ in 0..step.get() {
            let _ = machine.step().unwrap();
        }
        machine.witness_for_next_step()
    }

    /// A party that reports the truth up to a point and corrupted roots after it.
    struct Liar<'a> {
        honest: Replay<'a>,
        lies_from: u64,
    }

    impl Claimant for Liar<'_> {
        fn root_at(&mut self, step: Step) -> Result<Option<Hash>, Absent> {
            let truth = self.honest.root_at(step)?;
            if step.get() < self.lies_from {
                return Ok(truth);
            }
            Ok(truth.map(|mut root| {
                root[0] ^= 0xff;
                root
            }))
        }
    }

    #[test]
    fn a_witness_reproduces_what_the_original_machine_does_next() {
        // The foundation everything else rests on. At every step of a real execution, a
        // witness captured there must let a machine rebuilt from nothing else produce exactly
        // the state the original machine reaches by continuing.
        let module = canonical(ECHO);
        let image = crate::engine::image::decode(&module).unwrap();
        let input = b"witness me";

        let mut original = Machine::new(&image, input.to_vec(), Limits::default()).unwrap();
        let total = original.clone().run().unwrap().steps;
        assert!(total > 10, "the test module should run for a while");

        for step in 0..total {
            let witness = original.witness_for_next_step();

            let mut expected = original.clone();
            let _ = expected.step().unwrap();
            let expected_root = expected.commit().root();

            let mut rebuilt = Machine::restore(&image, &witness, input.to_vec(), Limits::default())
                .unwrap_or_else(|e| panic!("restore failed at step {step}: {e}"));
            let _ = rebuilt
                .step()
                .unwrap_or_else(|e| panic!("step failed at step {step}: {e}"));

            assert_eq!(
                rebuilt.commit().root(),
                expected_root,
                "a witness at step {step} did not reproduce the next state"
            );

            let _ = original.step().unwrap();
        }
    }

    #[test]
    fn a_witness_carries_only_the_pages_its_instruction_needs() {
        // The claim that makes adjudication affordable. A workload with a large memory must
        // still produce witnesses carrying a couple of pages, because one instruction can only
        // reach so far.
        let module = canonical(
            r#"(module
                 (import "cairn" "output" (func $output (param i32 i32)))
                 (memory (export "memory") 64 512)
                 (func (export "cairn_run") (local $i i32)
                   (block $done
                     (loop $again
                       (br_if $done (i32.ge_u (local.get $i) (i32.const 2000)))
                       (i32.store (local.get $i) (local.get $i))
                       (local.set $i (i32.add (local.get $i) (i32.const 4)))
                       (br $again)))
                   (call $output (i32.const 0) (i32.const 8))))"#,
        );
        let image = crate::engine::image::decode(&module).unwrap();

        let mut machine = Machine::new(&image, Vec::new(), Limits::default()).unwrap();
        let mut widest = 0;
        for _ in 0..400 {
            widest = widest.max(machine.witness_for_next_step().pages.len());
            if matches!(
                machine.step().unwrap(),
                crate::engine::machine::Progress::Finished
            ) {
                break;
            }
        }
        assert!(
            widest <= 2,
            "an instruction needed {widest} pages; a store spans at most two"
        );
    }

    #[test]
    fn adjudication_convicts_the_party_that_lied() {
        let module = canonical(ECHO);
        let image = crate::engine::image::decode(&module).unwrap();
        let input = b"convict";

        let mut probe = Machine::new(&image, input.to_vec(), Limits::default()).unwrap();
        let length = Step::new(probe.run().unwrap().steps);

        let mut honest = Replay::new(&image, input.to_vec(), Limits::default());
        let mut liar = Liar {
            honest: Replay::new(&image, input.to_vec(), Limits::default()),
            lies_from: 7,
        };

        let verdict = resolve(&mut honest, &mut liar, length).expect("should settle");
        assert_eq!(
            verdict.divergence,
            Step::new(6),
            "the instruction from step 6 to 7 is where the lie starts"
        );

        let witness = witness_at(&image, input, verdict.divergence);
        let judgment = adjudicate(&image, &verdict, &witness, input, Limits::default()).unwrap();
        assert_eq!(
            judgment,
            Judgment::Guilty {
                liar: Party::Second
            }
        );
    }

    #[test]
    fn adjudication_convicts_whichever_side_lies() {
        // Order must not matter. Swapping the parties must swap the verdict, not preserve it.
        let module = canonical(ECHO);
        let image = crate::engine::image::decode(&module).unwrap();
        let input = b"either way";

        let mut probe = Machine::new(&image, input.to_vec(), Limits::default()).unwrap();
        let length = Step::new(probe.run().unwrap().steps);

        let mut liar = Liar {
            honest: Replay::new(&image, input.to_vec(), Limits::default()),
            lies_from: 9,
        };
        let mut honest = Replay::new(&image, input.to_vec(), Limits::default());

        let verdict = resolve(&mut liar, &mut honest, length).unwrap();
        let witness = witness_at(&image, input, verdict.divergence);
        assert_eq!(
            adjudicate(&image, &verdict, &witness, input, Limits::default()).unwrap(),
            Judgment::Guilty { liar: Party::First }
        );
    }

    #[test]
    fn a_witness_from_the_wrong_state_is_refused() {
        // The check that makes a witness trustworthy: it must reconstruct the root bisection
        // already established. A witness captured one instruction later describes a real
        // state, just not the agreed one.
        let module = canonical(ECHO);
        let image = crate::engine::image::decode(&module).unwrap();
        let input = b"wrong state";

        let mut probe = Machine::new(&image, input.to_vec(), Limits::default()).unwrap();
        let length = Step::new(probe.run().unwrap().steps);

        let mut honest = Replay::new(&image, input.to_vec(), Limits::default());
        let mut liar = Liar {
            honest: Replay::new(&image, input.to_vec(), Limits::default()),
            lies_from: 8,
        };
        let verdict = resolve(&mut honest, &mut liar, length).unwrap();

        let wrong = witness_at(&image, input, Step::new(verdict.divergence.get() + 1));
        assert_eq!(
            adjudicate(&image, &verdict, &wrong, input, Limits::default()).unwrap_err(),
            AdjudicationError::WitnessDoesNotMatchAgreedState
        );
    }

    #[test]
    fn tampering_with_a_witness_is_refused() {
        let module = canonical(ECHO);
        let image = crate::engine::image::decode(&module).unwrap();
        let input = b"tamper";

        let mut probe = Machine::new(&image, input.to_vec(), Limits::default()).unwrap();
        let length = Step::new(probe.run().unwrap().steps);

        let mut honest = Replay::new(&image, input.to_vec(), Limits::default());
        let mut liar = Liar {
            honest: Replay::new(&image, input.to_vec(), Limits::default()),
            lies_from: 10,
        };
        let verdict = resolve(&mut honest, &mut liar, length).unwrap();
        let genuine = witness_at(&image, input, verdict.divergence);

        // Altering the small state changes the commitment, so it no longer matches the agreed
        // root and never reaches the machine.
        let mut altered_globals = genuine.clone();
        altered_globals
            .operand_stack
            .push(crate::state::Value::I32(0xDEAD));
        assert_eq!(
            adjudicate(&image, &verdict, &altered_globals, input, Limits::default()).unwrap_err(),
            AdjudicationError::WitnessDoesNotMatchAgreedState
        );

        // Altering a page's bytes breaks its proof, which is caught before the commitment is
        // even reconstructed.
        if let Some(page) = genuine.pages.first() {
            let mut altered_page = genuine.clone();
            altered_page.pages[0].bytes[0] ^= 0xff;
            let error =
                adjudicate(&image, &verdict, &altered_page, input, Limits::default()).unwrap_err();
            assert_eq!(
                error,
                AdjudicationError::UnusableWitness(
                    crate::engine::machine::WitnessError::BadPageProof { page: page.index }
                )
            );
        }
    }

    #[test]
    fn a_witness_missing_a_page_the_instruction_needs_is_refused() {
        // Dropping a page is the subtle attack: every remaining proof still verifies, so the
        // failure has to surface when the instruction reaches for what is not there.
        let module = canonical(
            r#"(module
                 (import "cairn" "output" (func $output (param i32 i32)))
                 (memory (export "memory") 2 4)
                 (func (export "cairn_run")
                   (i32.store (i32.const 100) (i32.const 42))
                   (call $output (i32.const 100) (i32.const 4))))"#,
        );
        let image = crate::engine::image::decode(&module).unwrap();

        // Walk to the store, whose witness carries the page it writes.
        let mut machine = Machine::new(&image, Vec::new(), Limits::default()).unwrap();
        let mut found = None;
        for step in 0..200u64 {
            let witness = machine.witness_for_next_step();
            if !witness.pages.is_empty() {
                found = Some((Step::new(step), witness));
                break;
            }
            if matches!(
                machine.step().unwrap(),
                crate::engine::machine::Progress::Finished
            ) {
                break;
            }
        }
        let (step, full) = found.expect("some instruction must touch memory");

        let mut stripped = full.clone();
        stripped.pages.clear();

        // The stripped witness still describes the agreed state — the memory root is carried
        // separately — so it passes the commitment check and fails at execution.
        let mut before = Machine::new(&image, Vec::new(), Limits::default()).unwrap();
        for _ in 0..step.get() {
            let _ = before.step().unwrap();
        }
        let agreed = before.commit().root();
        assert_eq!(stripped.commitment().root(), agreed);

        let verdict = Verdict {
            divergence: step,
            agreed_root: Some(agreed),
            first_claim: Some([1; 32]),
            second_claim: Some([2; 32]),
            rounds: 0,
        };

        // It gets as far as executing, and fails there: reaching for a page nobody supplied
        // must be an explicit failure and never a read of zeroes.
        match adjudicate(&image, &verdict, &stripped, &[], Limits::default()).unwrap_err() {
            AdjudicationError::ExecutionFailed(Trap::WitnessIncomplete { .. }) => {}
            other => panic!("expected the missing page to be caught, got {other:?}"),
        }
    }

    #[test]
    fn adjudication_reports_when_both_parties_are_wrong() {
        let module = canonical(ECHO);
        let image = crate::engine::image::decode(&module).unwrap();
        let input = b"both";

        let mut before = Machine::new(&image, input.to_vec(), Limits::default()).unwrap();
        for _ in 0..5 {
            let _ = before.step().unwrap();
        }
        let agreed = before.commit().root();
        let witness = witness_at(&image, input, Step::new(5));

        let verdict = Verdict {
            divergence: Step::new(5),
            agreed_root: Some(agreed),
            first_claim: Some([7; 32]),
            second_claim: Some([9; 32]),
            rounds: 1,
        };

        match adjudicate(&image, &verdict, &witness, input, Limits::default()).unwrap() {
            Judgment::BothWrong { actual } => assert!(actual.is_some()),
            other => panic!("expected both wrong, got {other:?}"),
        }
    }

    #[test]
    fn every_adjudication_error_renders_a_message() {
        let samples = [
            AdjudicationError::WitnessDoesNotMatchAgreedState,
            AdjudicationError::UnusableWitness(crate::engine::machine::WitnessError::NoFrames),
            AdjudicationError::ExecutionFailed(Trap::Unreachable),
            AdjudicationError::NoAgreedState,
        ];
        for sample in samples {
            assert!(
                !sample.to_string().is_empty(),
                "empty message for {sample:?}"
            );
        }
    }

    #[test]
    fn every_error_renders_a_message() {
        let samples = [
            DisputeError::NoDisagreement,
            DisputeError::DisagreeAtStart,
            DisputeError::EmptyExecution,
            DisputeError::Abandoned {
                by: Party::First,
                at: Step::new(1),
                rounds: 2,
            },
            DisputeError::DidNotConverge,
        ];
        for sample in samples {
            assert!(
                !sample.to_string().is_empty(),
                "empty message for {sample:?}"
            );
        }
    }
}
