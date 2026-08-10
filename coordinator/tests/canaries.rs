//! How long a cheat lasts, measured rather than assumed.
//!
//! [ADR-0001](../../docs/adr/0001-verification-by-dispute-not-replication.md) rests on a canary
//! rate `c` and asserts, without a number anywhere, that sampling catches dishonest volunteers.
//! This is the number. It injects volunteers that return wrong answers at a known rate and
//! reports how many units each of them completed before the coordinator caught it.
//!
//! # Why this is a simulation and not a live test
//!
//! Because the question is statistical and the answer has to be a distribution, not an anecdote.
//! Ten thousand leases against the real [`Grid`] — real canary minting, real reputation, the
//! real dispatch path — is the only way to say "a volunteer cheating on one unit in ten is
//! caught after about N units" with any confidence. Nothing here is mocked; the only thing
//! standing in for reality is the volunteer, which is a coin flip.
//!
//! The RNG is a seeded xorshift, so a failure is reproducible rather than a flake, and the
//! printed table is identical from run to run.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::print_stdout)]

use std::time::Instant;

use cairn_coordinator::grid::{Grid, Outcome, Submission, DEFAULT_REPLICATION_PERCENT};
use cairn_coordinator::reputation::{Policy, Reputation, Standing};

const WORKLOAD: &str = r#"
    (module
      (import "cairn" "input"  (func $input  (param i32 i32) (result i32)))
      (import "cairn" "output" (func $output (param i32 i32)))
      (memory (export "memory") 1 1)
      (func (export "cairn_run") (local $n i32)
        (local.set $n (call $input (i32.const 0) (i32.const 0)))
        (i32.store (i32.const 0) (local.get $n))
        (call $output (i32.const 0) (i32.const 4))))
"#;

/// The honest answer for an input of this length: the workload writes its own input's length.
fn honest(input_len: usize) -> Vec<u8> {
    (input_len as u32).to_le_bytes().to_vec()
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// True with probability `permille / 1000`.
    fn happens(&mut self, permille: u32) -> bool {
        (self.next() % 1000) < u64::from(permille)
    }
}

/// A grid with `units` pieces of work and replication at its default rate.
///
/// **Replication cannot be turned off here, and finding that out was the point.** A canary is a
/// copy of a unit whose answer the coordinator is sure of, and single-execution acceptance is
/// not sure of anything — so canaries are minted only from corroborated units, and corroboration
/// comes from replication. `r = 0` means no canaries at all.
///
/// What is still being measured cleanly is *the canaries*: these volunteers declare `bisects:
/// false`, so a disagreement takes the re-execution route, which settles the unit and updates no
/// reputation. Nothing but a failed canary marks a worker as proven wrong.
fn grid_with(units: usize, policy: Policy) -> (Grid, String) {
    let mut grid = Grid::new()
        .with_replication(DEFAULT_REPLICATION_PERCENT)
        .with_reputation(Reputation::new(policy));
    let id = grid
        .register("test", WORKLOAD.as_bytes())
        .expect("admissible");
    for n in 0..units {
        grid.submit(&id, vec![b'x'; (n % 64) + 1]).expect("queued");
    }
    (grid, id)
}

/// Run one volunteer against the grid until it is caught or runs out of work.
///
/// Returns how many units it completed before the coordinator first knew it was wrong, and
/// `None` if it was never caught.
fn units_until_caught(
    grid: &mut Grid,
    worker: &str,
    cheats_permille: u32,
    rng: &mut Rng,
    give_up_after: usize,
) -> Option<usize> {
    let mut done = 0;
    for _ in 0..give_up_after {
        let now = Instant::now();
        let Some(assignment) = grid.lease(worker, now) else {
            break;
        };

        let output = if rng.happens(cheats_permille) {
            vec![0xde, 0xad, 0xbe, 0xef]
        } else {
            honest(assignment.input.len())
        };

        let _ = grid.submit_result(
            assignment.unit,
            Submission {
                worker: worker.to_owned(),
                output,
                fuel: None,
                bisects: false,
            },
        );
        done += 1;

        if matches!(
            grid.reputation().standing(worker),
            Standing::ProvenWrong { .. }
        ) {
            return Some(done);
        }
    }
    None
}

/// Seed the grid with *corroborated* work, so there is something for canaries to be drawn from.
///
/// Each honest volunteer takes a few units; the replicated ones among them collect a second
/// agreeing answer and become the ground truth every canary is copied from. A brand-new grid can
/// mint no canaries at all, which is not a bug — it is the bootstrap, and in a real deployment it
/// settles itself within the first few dozen units.
fn warm_up(grid: &mut Grid, rounds: usize) {
    let mut rng = Rng(1);
    for n in 0..rounds {
        let worker = format!("seed-{n}");
        units_until_caught(grid, &worker, 0, &mut rng, 8);
    }
}

#[test]
fn a_volunteer_that_always_cheats_is_caught_within_a_handful_of_units() {
    let mut rng = Rng(0x1234_5678_9abc_def0);
    let (mut grid, _) = grid_with(4000, Policy::default());
    warm_up(&mut grid, 20);

    let mut caught = Vec::new();
    for trial in 0..40 {
        let worker = format!("always-{trial}");
        let after = units_until_caught(&mut grid, &worker, 1000, &mut rng, 500);
        caught.push(after.expect("a volunteer cheating on every unit must be caught"));
    }

    caught.sort_unstable();
    let median = caught.get(caught.len() / 2).copied().unwrap_or(0);
    let worst = caught.last().copied().unwrap_or(0);
    println!(
        "always cheats: median {median} units, worst {worst}, n = {}",
        caught.len()
    );

    // A worker that is wrong about everything never passes a canary, so it never becomes
    // trusted, so it stays on the high sampling rate — 250 permille by default, which is one
    // unit in four. The bound is loose because this asserts the mechanism works, not that the
    // arithmetic came out to any particular value.
    assert!(
        median <= 10,
        "median {median} units to catch somebody wrong about everything"
    );
}

#[test]
fn the_cost_of_cheating_rarely_is_that_it_takes_longer_to_get_caught() {
    // **The table this test exists to produce.** ADR-0001 assumes sampling works and never says
    // how fast. A volunteer that cheats on one unit in a hundred is doing real damage — a
    // hundredth of the network's science is wrong — and the honest answer to "how long does it
    // last" is the difference between a design that works and one that only sounds like it.
    let mut rows = Vec::new();

    for cheats_permille in [1000u32, 500, 200, 100, 50, 10] {
        let mut rng = Rng(0xfeed_face_dead_beef ^ u64::from(cheats_permille));
        let (mut grid, _) = grid_with(30_000, Policy::default());
        warm_up(&mut grid, 20);

        let mut caught = Vec::new();
        let mut escaped = 0;
        for trial in 0..30 {
            let worker = format!("p{cheats_permille}-{trial}");
            match units_until_caught(&mut grid, &worker, cheats_permille, &mut rng, 900) {
                Some(after) => caught.push(after),
                None => {
                    escaped += 1;
                    // Counted at the horizon rather than dropped. A median over only the ones
                    // that were caught is a median over the unlucky, and it flatters the
                    // mechanism exactly where the mechanism is weakest.
                    caught.push(900);
                }
            }
        }

        caught.sort_unstable();
        let median = caught.get(caught.len() / 2).copied().unwrap_or(0);
        let wrong_answers_delivered = median * cheats_permille as usize / 1000;
        rows.push((cheats_permille, median, wrong_answers_delivered, escaped));
    }

    println!();
    println!("  cheat rate   units until caught   wrong answers accepted   never caught in 900");
    println!("  ----------   ------------------   ----------------------   -------------------");
    for (permille, median, delivered, escaped) in &rows {
        println!(
            "  {:>7}‰   {:>18}   {:>22}   {:>19}",
            permille, median, delivered, escaped
        );
    }
    println!();
    println!("  Median over 30 volunteers each, canary policy at its defaults:");
    println!("  30‰ once trusted, 250‰ until then, and nine clean canaries to become");
    println!("  trusted — which is where a Beta(1,1) prior first passes a 900‰ threshold.");
    println!("  Volunteers still uncaught at 900 units are counted AS 900, so the last rows");
    println!("  are lower bounds — the real figure for 10‰ is worse than the one printed.");
    println!();

    // The shape is the finding, and it is not the flattering one: **cheating less often buys a
    // cheat more time, and the number of wrong answers it lands before being caught is roughly
    // flat.** Sampling bounds the damage per cheat, it does not bound the time.
    let always = rows.first().expect("a row for 1000 permille");
    let rarely = rows.last().expect("a row for 10 permille");
    assert!(
        rarely.1 > always.1 * 4,
        "cheating rarely should take much longer to catch: {} vs {}",
        rarely.1,
        always.1
    );
}

#[test]
fn an_honest_volunteer_is_never_caught_and_stops_being_watched_so_hard() {
    // The other half, and the one that would be a disaster to get wrong. A false positive here
    // is an honest volunteer marked as a cheat by the coordinator's own bookkeeping.
    let mut rng = Rng(0x0bad_c0de_0bad_c0de);
    let (mut grid, _) = grid_with(4000, Policy::default());
    warm_up(&mut grid, 20);

    let caught = units_until_caught(&mut grid, "honest", 0, &mut rng, 400);
    assert_eq!(caught, None, "an honest volunteer was marked as a cheat");

    let record = grid.reputation().record("honest");
    assert!(
        record.passed >= Policy::default().proving_canaries,
        "an honest volunteer never got enough canaries to earn trust: {record:?}"
    );
    assert!(matches!(
        grid.reputation().standing("honest"),
        Standing::Trusted { .. }
    ));

    // And it is now checked at the rate ADR-0001's cost model assumes rather than the rate a
    // stranger gets. That difference is the whole of "selective" in selective sampling.
    assert_eq!(
        grid.reputation().canary_permille("honest"),
        Policy::default().canaries_when_trusted
    );
    assert_eq!(
        grid.reputation().canary_permille("stranger"),
        Policy::default().canaries_when_not
    );

    println!(
        "honest volunteer after 400 units: {:?}, canary rate {}‰",
        record,
        grid.reputation().canary_permille("honest")
    );
}

#[test]
fn a_canary_is_not_distinguishable_from_the_work_it_was_drawn_from() {
    // The assumption the whole mechanism rests on, and the one ADR-0001 flags as its limit: "a
    // canary sampling scheme is defeated by an adversary who can distinguish canaries from real
    // work." What a volunteer sees is a unit index, a workload id and an input. The first must
    // not be a decided unit's index — `/api/status` is public and says which those are — and the
    // other two are copied from live work by construction.
    let (mut grid, id) = grid_with(200, Policy::default());
    warm_up(&mut grid, 30);

    let mut saw_a_canary = false;
    for n in 0..200 {
        let worker = format!("watcher-{n}");
        let Some(assignment) = grid.lease(&worker, Instant::now()) else {
            continue;
        };
        let unit = grid.unit(assignment.unit).expect("leased unit exists");
        if unit.canary.is_none() {
            continue;
        }
        saw_a_canary = true;

        assert_eq!(
            assignment.workload, id,
            "a canary named a different workload"
        );
        assert!(
            !assignment.input.is_empty(),
            "a canary carried an input no live unit would have"
        );
        assert_eq!(
            unit.outcome,
            Outcome::Open,
            "a canary was already decided, which /api/status would announce"
        );
        let source = unit.canary.as_ref().unwrap().source;
        assert_ne!(
            assignment.unit, source,
            "a canary reused its source's index, which /api/status reports as accepted"
        );
        assert!(
            assignment.unit >= 200,
            "a canary took an index inside the original queue"
        );
        assert!(
            grid.unit(source)
                .map(|s| s.results.len() >= 2)
                .unwrap_or(false),
            "a canary was drawn from a unit only one volunteer ever answered"
        );
    }
    assert!(
        saw_a_canary,
        "no canary was minted, so this test checked nothing"
    );
}
