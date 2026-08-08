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
use crate::engine::machine::{Limits, Machine, Progress};
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

/// A party that answers by re-running its own execution.
///
/// # Cost
///
/// Each answer replays from the beginning, so a full bisection costs a party `O(n log n)`. A
/// production worker would keep periodic checkpoints — the full state, not just the roots the
/// trace commits to — and restart from the nearest one, bringing it to `O(n)`. That is an
/// optimisation of one party's bookkeeping and changes nothing about the protocol, which is
/// why it is not here.
pub struct Replay<'a> {
    image: &'a Image<'a>,
    input: Vec<u8>,
    limits: Limits,
}

impl<'a> Replay<'a> {
    /// A party that will replay `image` on `input` to answer.
    #[must_use]
    pub fn new(image: &'a Image<'a>, input: Vec<u8>, limits: Limits) -> Self {
        Self {
            image,
            input,
            limits,
        }
    }
}

impl Claimant for Replay<'_> {
    fn root_at(&mut self, step: Step) -> Result<Option<Hash>, Absent> {
        let Ok(mut machine) = Machine::new(self.image, self.input.clone(), self.limits) else {
            return Ok(None);
        };

        for _ in 0..step.get() {
            match machine.step() {
                // Execution ended. If it ended exactly here, this is the final state;
                // otherwise the requested step is past the end.
                Ok(Progress::Finished) => {
                    return Ok((machine.steps() == step.get()).then(|| machine.commit().root()));
                }
                Ok(_) => {}
                // A trapped execution has no state at or after the trap.
                Err(_) => return Ok(None),
            }
        }
        Ok(Some(machine.commit().root()))
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
