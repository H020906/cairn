//! The bisection game, checked exhaustively rather than by example.
//!
//! # Why this file can be exhaustive when nothing else here can
//!
//! [`Challenge`] is a **pure state machine**. It holds two bounds and a round count; it has no
//! interpreter, no clock, no network and no I/O. That means the whole space of interesting
//! inputs is `(execution length, divergence point)`, and for lengths up to a few hundred that
//! space can be walked *completely*.
//!
//! Which is worth doing, because this is the one part of Cairn where a bug would not look like
//! a bug. A wrong answer from the interpreter shows up as a differential failure. A wrong
//! answer from the bisection shows up as **an honest volunteer being convicted**, quietly,
//! rarely, in a run nobody is watching.
//!
//! The properties, in the order they matter:
//!
//! 1. **It converges on the right instruction.** For every length `n` and every divergence
//!    point `d < n`, the game settles on exactly `d`. Not near it.
//! 2. **It converges in `log₂ n` rounds**, which is the claim the coordinator's cost rests on.
//! 3. **It cannot get stuck.** Every reachable state either asks a question strictly inside its
//!    own bracket or declares itself settled.
//! 4. **It does not care which party is which.** Swapping them changes nothing.
//! 5. **A party that stops answering is named**, not waited for.
//!
//! # The round count is a band, not a number
//!
//! Documentation throughout this project says `⌈log₂ n⌉`, and that is the right thing to say —
//! but it is the **worst case over divergence points**, not the count for every one. A round
//! either moves the floor up (halving the gap, rounding up) or the ceiling down (rounding
//! down), so where `d` falls decides whether the last round is needed. The real statement is
//! `⌈log₂ n⌉ - 1 ≤ rounds ≤ ⌈log₂ n⌉`, with the upper end attained. Both halves are asserted
//! below, because a test that pinned only the upper bound would pass on a state machine that
//! had silently stopped halving.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use cairn_runtime::dispute::{self, Absent, Challenge, Claimant, DisputeError, Party, Round, Step};
use cairn_runtime::merkle::Hash;

/// A party whose execution matches the other up to `diverged_at` and differs after it.
///
/// The whole model of a disputing worker, and it is this small for a real reason: **the
/// coordinator never sees anything but roots.** Whether the divergence came from a liar, a
/// failing memory chip or an overclocked CPU is invisible here, and the protocol is designed
/// not to need it.
struct Diverged {
    diverged_at: u64,
    /// Distinguishes the two parties after the divergence. Before it, both return the same
    /// thing, because before it they really did compute the same thing.
    marker: u8,
    /// Answers `Absent` from this step onward, modelling a party that stops responding.
    silent_from: Option<u64>,
    /// Steps this party was actually asked about, in order.
    asked: Vec<u64>,
}

impl Diverged {
    fn new(diverged_at: u64, marker: u8) -> Self {
        Self {
            diverged_at,
            marker,
            silent_from: None,
            asked: Vec::new(),
        }
    }

    fn silent_from(mut self, step: u64) -> Self {
        self.silent_from = Some(step);
        self
    }
}

impl Claimant for Diverged {
    fn root_at(&mut self, step: Step) -> Result<Option<Hash>, Absent> {
        self.asked.push(step.get());

        if self.silent_from.is_some_and(|from| step.get() >= from) {
            return Err(Absent);
        }

        let mut root = [0u8; 32];
        root[..8].copy_from_slice(&step.get().to_le_bytes());
        if step.get() > self.diverged_at {
            root[31] = self.marker;
        }
        Ok(Some(root))
    }
}

/// Smallest `r` with `2^r >= n`. The worst-case round count over all divergence points.
fn ceil_log2(n: u64) -> u32 {
    if n <= 1 {
        return 0;
    }
    64 - (n - 1).leading_zeros()
}

/// Play a whole game and return `(divergence, rounds)`, checking the invariants every round.
///
/// The invariant checks are here rather than in their own test because they are properties of
/// *every reachable state*, and the only way to reach every state is to play.
#[track_caller]
fn play(first: &mut impl Claimant, second: &mut impl Claimant, length: u64) -> (u64, u32) {
    let mut challenge = Challenge::open(Step::new(length)).expect("a non-empty execution");
    let mut previous = challenge.bounds();

    loop {
        let (low, high) = challenge.bounds();

        // The bracket never inverts and never widens. If either could, the search would be
        // free to wander instead of converging.
        assert!(low.get() < high.get(), "bracket inverted: [{low}, {high}]");
        assert!(
            low.get() >= previous.0.get() && high.get() <= previous.1.get(),
            "bracket widened: [{}, {}] -> [{low}, {high}]",
            previous.0,
            previous.1
        );
        previous = (low, high);

        match challenge.round() {
            Round::Ask { step } => {
                // Asking about an endpoint would waste a round: the answer is already known
                // there, so the bracket could not narrow and the game could not terminate.
                assert!(
                    step.get() > low.get() && step.get() < high.get(),
                    "asked about {step}, outside its own bracket [{low}, {high}]"
                );
                assert!(
                    challenge.rounds() < dispute::MAX_ROUNDS,
                    "exceeded MAX_ROUNDS, which means the machine stopped making progress"
                );

                let a = first.root_at(step).expect("present");
                let b = second.root_at(step).expect("present");
                challenge.record(a, b);
            }
            Round::Settled { divergence } => return (divergence.get(), challenge.rounds()),
        }
    }
}

// --- 1 and 2: convergence and cost, exhaustively --------------------------------------------

/// Every length up to 512, and **every** divergence point within each. Not sampled.
///
/// About 131,000 games. It runs in well under a second because the state machine is four
/// integers, which is the argument for testing it this way rather than with a property-based
/// framework and a hundred random cases.
#[test]
fn every_divergence_point_of_every_short_execution_is_found_exactly() {
    for length in 1..=512u64 {
        let worst = ceil_log2(length);
        let mut worst_seen = 0;

        for diverged_at in 0..length {
            let mut first = Diverged::new(diverged_at, 0);
            let mut second = Diverged::new(diverged_at, 1);
            let (found, rounds) = play(&mut first, &mut second, length);

            assert_eq!(
                found, diverged_at,
                "length {length}, divergence {diverged_at}: settled on {found}"
            );
            assert!(
                rounds <= worst,
                "length {length}, divergence {diverged_at}: {rounds} rounds exceeds ⌈log₂ n⌉ = {worst}"
            );
            assert!(
                rounds + 1 >= worst,
                "length {length}, divergence {diverged_at}: {rounds} rounds is below the band, \
                 so the search is no longer halving"
            );
            worst_seen = worst_seen.max(rounds);
        }

        // The documented figure is the worst case, and it must actually be attained — otherwise
        // every claim of `⌈log₂ n⌉` in this repository is quietly pessimistic.
        assert_eq!(
            worst_seen, worst,
            "length {length}: worst case over all divergence points was {worst_seen}, \
             not ⌈log₂ n⌉ = {worst}"
        );
    }
}

/// The same property where the space cannot be walked: executions up to 2^63 instructions.
///
/// A real work unit is millions of instructions, not hundreds, and the arithmetic that keeps
/// the search away from overflow at the top of the `u64` range is only exercised up here.
#[test]
fn enormous_executions_converge_in_log_rounds() {
    let mut rng = Rng(0x5eed_1234_abcd_0001);

    for _ in 0..20_000 {
        // Lengths spread across the whole exponent range rather than clustered near the top,
        // so this covers the awkward small-but-not-tiny sizes as well as the extremes.
        let bits = 1 + rng.pick(63);
        let length = 1 + (rng.next() % (1u64 << bits));
        let diverged_at = rng.next() % length;

        let mut first = Diverged::new(diverged_at, 0);
        let mut second = Diverged::new(diverged_at, 1);
        let (found, rounds) = play(&mut first, &mut second, length);

        assert_eq!(found, diverged_at, "length {length}");
        let worst = ceil_log2(length);
        assert!(
            rounds <= worst && rounds + 1 >= worst,
            "length {length}, divergence {diverged_at}: {rounds} rounds against ⌈log₂ n⌉ = {worst}"
        );
    }
}

/// A dispute costs the coordinator a number of questions, and that number must be *questions*,
/// not questions-per-instruction.
///
/// Stated separately because it is the claim in the README rather than a property of the
/// bounds: a million-fold longer execution costs about twenty more rounds, not a million times
/// as many.
#[test]
fn a_thousandfold_longer_execution_costs_ten_more_rounds() {
    let cost = |length: u64| {
        let diverged_at = length - 1; // the expensive shape: divergence at the very end
        let mut first = Diverged::new(diverged_at, 0);
        let mut second = Diverged::new(diverged_at, 1);
        play(&mut first, &mut second, length).1
    };

    let small = cost(1_000);
    let large = cost(1_000_000);
    let huge = cost(1_000_000_000);

    assert_eq!((small, large, huge), (10, 20, 30));
}

// --- 3: the questions asked are the ones a coordinator would have to store -------------------

/// Questions asked, exactly: one per round, plus a fixed opening and settlement.
///
/// Worth pinning because the obvious implementation of a bisection — keeping the answers —
/// would make the coordinator's memory grow with `log n` per open dispute, and the design claim
/// is that it does not grow at all. The count is `rounds + constant`, and the constant is
/// checked rather than described.
///
/// # The first party is asked one more question, and that is deliberate
///
/// At settlement the coordinator needs three things: the state both parties agreed on entering
/// the disputed instruction, and each party's claim about what it became. **The agreed root is
/// taken from the first party alone**, because the bisection has already established that both
/// report the same value there — the lower bound only ever moves to a step where their answers
/// matched, and step 0 is checked separately before the search begins.
///
/// So the asymmetry is a saved round-trip, not a preference. A refactor that "fixed" it by
/// asking both parties would buy nothing and cost a message, and
/// [`swapping_the_parties_changes_nothing`] holds regardless.
#[test]
fn a_party_is_asked_once_per_round_plus_a_fixed_overhead() {
    let length = 10_000;
    let diverged_at = 7_777;

    let mut first = Diverged::new(diverged_at, 0);
    let mut second = Diverged::new(diverged_at, 1);
    let verdict = dispute::resolve(&mut first, &mut second, Step::new(length)).expect("settles");

    assert_eq!(verdict.divergence.get(), diverged_at);

    // Opening: step 0 and step `length`, to both parties. Settlement: the agreed root from the
    // first party, then the claim at `divergence + 1` from each.
    assert_eq!(
        second.asked.len() as u32,
        verdict.rounds + 3,
        "second party: {} questions for {} rounds",
        second.asked.len(),
        verdict.rounds
    );
    assert_eq!(
        first.asked.len(),
        second.asked.len() + 1,
        "the first party supplies the agreed root, so it answers exactly one question more"
    );

    // And the extra question is the one this doc-comment claims it is.
    let agreed_asked_of_second = second.asked.iter().filter(|&&s| s == diverged_at).count();
    let agreed_asked_of_first = first.asked.iter().filter(|&&s| s == diverged_at).count();
    assert_eq!(
        (agreed_asked_of_first, agreed_asked_of_second),
        (2, 1),
        "the divergence step is asked of the first party twice — once in the search, once for \
         the agreed root — and of the second only in the search"
    );
}

// --- 4: symmetry -----------------------------------------------------------------------------

/// Swapping the parties must not change the answer.
///
/// The state machine records `first == second` or `first != second` and nothing else, so this
/// should be trivially true — which is exactly why it is worth a test. An implementation that
/// grew a preference for one side would be a protocol that decides disputes by seating order.
#[test]
fn swapping_the_parties_changes_nothing() {
    for length in [7u64, 64, 1000, 65_536] {
        for diverged_at in [0, 1, length / 3, length - 1] {
            let mut a1 = Diverged::new(diverged_at, 0);
            let mut b1 = Diverged::new(diverged_at, 1);
            let forwards = play(&mut a1, &mut b1, length);

            let mut a2 = Diverged::new(diverged_at, 1);
            let mut b2 = Diverged::new(diverged_at, 0);
            let backwards = play(&mut b2, &mut a2, length);

            assert_eq!(
                forwards, backwards,
                "length {length}, divergence {diverged_at}"
            );
        }
    }
}

// --- 5: the ways a game can fail to be a game ------------------------------------------------

/// A party that stops answering is named, at the step it stopped, with the rounds so far.
///
/// The alternative — waiting — is how a distributed protocol becomes a distributed hang. The
/// coordinator needs a decision it can act on, and "the second party abandoned at step 4096
/// after 7 rounds" is one.
#[test]
fn a_silent_party_is_named_rather_than_waited_for() {
    let length = 4_096;

    for (silent, expected) in [(Party::First, Party::First), (Party::Second, Party::Second)] {
        let mut first = Diverged::new(2_000, 0);
        let mut second = Diverged::new(2_000, 1);
        // Silent from partway in, so the game gets going before it stalls — a party that never
        // answered at all would be caught by the opening exchange rather than mid-search.
        if silent == Party::First {
            first = first.silent_from(2_048);
        } else {
            second = second.silent_from(2_048);
        }

        match dispute::resolve(&mut first, &mut second, Step::new(length)) {
            Err(DisputeError::Abandoned { by, at, rounds }) => {
                assert_eq!(by, expected);
                assert!(
                    at.get() >= 2_048,
                    "abandoned at {at}, before it went silent"
                );
                assert!(rounds < dispute::MAX_ROUNDS);
            }
            other => panic!("expected an abandonment, got {other:?}"),
        }
    }
}

/// Two parties that agree everywhere are not a dispute, and saying so is not the same as
/// picking a winner.
#[test]
fn agreement_is_reported_rather_than_adjudicated() {
    let mut first = Diverged::new(u64::MAX, 0);
    let mut second = Diverged::new(u64::MAX, 1);
    assert!(matches!(
        dispute::resolve(&mut first, &mut second, Step::new(1_000)),
        Err(DisputeError::NoDisagreement)
    ));
}

/// Parties that differ at step zero disagree about the *unit*, not about an instruction in it.
///
/// There is nothing to bisect: no instruction produced the difference, so no instruction can
/// settle it. Returning a divergence anyway would name an instruction that is not to blame.
#[test]
fn disagreeing_before_the_first_instruction_is_refused() {
    struct Wrong(u8);
    impl Claimant for Wrong {
        fn root_at(&mut self, _step: Step) -> Result<Option<Hash>, Absent> {
            Ok(Some([self.0; 32]))
        }
    }

    assert!(matches!(
        dispute::resolve(&mut Wrong(1), &mut Wrong(2), Step::new(1_000)),
        Err(DisputeError::DisagreeAtStart)
    ));
}

/// A party that stopped early disagrees from the first step past its end.
///
/// `Ok(None)` means "my execution had ended", which is a legitimate answer and a different one
/// from a hash. Two workers that stop at different points must converge on the first
/// instruction only one of them executed.
#[test]
fn an_execution_that_ended_early_diverges_where_it_ended() {
    struct StopsAt(u64);
    impl Claimant for StopsAt {
        fn root_at(&mut self, step: Step) -> Result<Option<Hash>, Absent> {
            if step.get() > self.0 {
                return Ok(None);
            }
            let mut root = [0u8; 32];
            root[..8].copy_from_slice(&step.get().to_le_bytes());
            Ok(Some(root))
        }
    }

    let mut short = StopsAt(600);
    let mut long = StopsAt(1_000);
    let verdict = dispute::resolve(&mut short, &mut long, Step::new(1_000)).expect("settles");

    assert_eq!(
        verdict.divergence.get(),
        600,
        "the divergence is the last instruction they both executed"
    );
    assert_eq!(verdict.first_claim, None, "the short party had finished");
    assert!(verdict.second_claim.is_some());
}

/// A zero-length execution has no instruction to blame.
#[test]
fn an_empty_execution_has_nothing_to_bisect() {
    assert!(Challenge::open(Step::ZERO).is_none());
    let mut first = Diverged::new(0, 0);
    let mut second = Diverged::new(0, 1);
    assert!(matches!(
        dispute::resolve(&mut first, &mut second, Step::ZERO),
        Err(DisputeError::EmptyExecution)
    ));
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn pick(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}
