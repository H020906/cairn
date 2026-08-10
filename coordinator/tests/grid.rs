//! What the coordinator decides, checked without opening a socket.
//!
//! The coordinator is the one component that can convict an honest volunteer, so its decisions
//! are kept in `grid.rs` with no HTTP in them and tested here directly. Anything that needed a
//! server to test would be a decision in the wrong file.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::time::{Duration, Instant};

use cairn_coordinator::grid::{Grid, Outcome, Refusal, Submission};

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

/// The workload writes its input's length, so the honest answer is knowable from the test.
fn expected(input_len: u32) -> Vec<u8> {
    input_len.to_le_bytes().to_vec()
}

fn grid_with_one_unit(input: &[u8]) -> (Grid, usize) {
    let mut grid = Grid::new().with_replication(0);
    let id = grid
        .register("test", WORKLOAD.as_bytes())
        .expect("admissible");
    let unit = grid.submit(&id, input.to_vec()).expect("queued");
    (grid, unit)
}

/// An answer from a volunteer that cannot argue about it — a browser, in other words.
///
/// The common case, and the reason `bisects` defaults to false everywhere: a volunteer that
/// executes on the engine it already has cannot produce a state root, so challenging it would
/// convict it for silence. See `Submission::bisects`.
fn answer(worker: &str, output: Vec<u8>) -> Submission {
    Submission {
        worker: worker.to_owned(),
        output,
        fuel: None,
        bisects: false,
    }
}

#[test]
fn a_unit_is_accepted_after_one_execution() {
    // The project's entire claim, as a test. Not two executions, not two-of-three: one.
    let (mut grid, unit) = grid_with_one_unit(b"abcde");

    let assignment = grid.lease("alice", Instant::now()).expect("work available");
    assert_eq!(assignment.unit, unit);

    let outcome = grid
        .submit_result(unit, answer("alice", expected(5)))
        .expect("accepted");

    assert_eq!(
        outcome,
        Outcome::Accepted {
            output: expected(5)
        }
    );
}

#[test]
fn a_replicated_unit_waits_for_a_second_opinion() {
    let mut grid = Grid::new().with_replication(100); // every unit replicated
    let id = grid.register("test", WORKLOAD.as_bytes()).unwrap();
    let unit = grid.submit(&id, b"abcde".to_vec()).unwrap();

    let now = Instant::now();
    grid.lease("alice", now).expect("work");
    assert_eq!(
        grid.submit_result(unit, answer("alice", expected(5)))
            .unwrap(),
        Outcome::Open,
        "one result is not a quorum of two"
    );

    grid.lease("bob", now).expect("work");
    assert_eq!(
        grid.submit_result(unit, answer("bob", expected(5)))
            .unwrap(),
        Outcome::Accepted {
            output: expected(5)
        }
    );
}

#[test]
fn the_same_worker_is_never_given_the_same_unit_twice() {
    // **The load-bearing line in the whole scheduler.** A quorum of two means two *independent*
    // executions; a unit replicated back to the same machine is the same execution counted
    // twice, and it would agree with itself no matter how broken that machine was.
    let mut grid = Grid::new().with_replication(100);
    let id = grid.register("test", WORKLOAD.as_bytes()).unwrap();
    let unit = grid.submit(&id, b"abcde".to_vec()).unwrap();

    let now = Instant::now();
    assert!(grid.lease("alice", now).is_some());
    assert!(
        grid.lease("alice", now).is_none(),
        "alice holds a lease and must not be given the same unit again"
    );

    grid.submit_result(unit, answer("alice", expected(5)))
        .unwrap();
    assert!(
        grid.lease("alice", now).is_none(),
        "alice has answered and must not be asked to confirm herself"
    );
    assert!(
        grid.lease("bob", now).is_some(),
        "the second opinion must be available to somebody else"
    );
}

#[test]
fn a_worker_using_every_core_holds_many_leases_and_still_gets_one_vote_on_each() {
    // The scheduler's half of multi-core volunteering, and the reason a sixteen-thread machine
    // reports under **one** name rather than sixteen.
    //
    // Both halves have to hold at once. A machine that could not hold several leases would waste
    // fifteen cores; a machine that could hold two leases *on the same unit* would satisfy a
    // quorum of two by itself, and the "two independent executions" a replicated unit is supposed
    // to buy would be one machine agreeing with itself. Registering as sixteen workers is exactly
    // how somebody would get the first without noticing they had lost the second.
    let mut grid = Grid::new().with_replication(100);
    let id = grid.register("test", WORKLOAD.as_bytes()).unwrap();
    let units: Vec<usize> = (1..=4)
        .map(|n| grid.submit(&id, vec![b'x'; n]).unwrap())
        .collect();

    let now = Instant::now();
    let mut leased = Vec::new();
    while let Some(assignment) = grid.lease("one-machine", now) {
        leased.push(assignment.unit);
    }

    assert_eq!(leased.len(), units.len(), "every unit should be workable");
    leased.sort_unstable();
    assert_eq!(leased, units, "and each of them exactly once");

    // Answering them all does not make this machine eligible to confirm any of them.
    for (n, unit) in units.iter().enumerate() {
        grid.submit_result(*unit, answer("one-machine", expected(n as u32 + 1)))
            .unwrap();
    }
    assert!(
        grid.lease("one-machine", now).is_none(),
        "a quorum of two must not be reachable by one machine, however many cores it has"
    );
    assert!(
        grid.lease("another-machine", now).is_some(),
        "the second opinion is still owed to somebody else"
    );
}

#[test]
fn a_disagreement_between_parties_that_cannot_argue_is_settled_by_re_execution() {
    // The fallback route, and it is a route rather than a gap. Neither of these volunteers can
    // answer a challenge — that is what `bisects: false` says — so there is nobody to bisect
    // against and the referee does the work itself. `tests/interactive.rs` covers the other
    // route, where the parties argue and the referee executes one instruction.
    let mut grid = Grid::new().with_replication(100);
    let id = grid.register("test", WORKLOAD.as_bytes()).unwrap();
    let unit = grid.submit(&id, b"abcde".to_vec()).unwrap();

    let now = Instant::now();
    grid.lease("honest", now).unwrap();
    grid.lease("liar", now).unwrap();

    grid.submit_result(unit, answer("honest", expected(5)))
        .unwrap();
    let outcome = grid
        .submit_result(unit, answer("liar", vec![9, 9, 9, 9]))
        .unwrap();

    match outcome {
        Outcome::Settled {
            verdict, output, ..
        } => {
            assert!(
                verdict.contains("second party"),
                "the liar answered second, so it is the second party: {verdict}"
            );
            assert_eq!(
                output,
                Some(expected(5)),
                "the referee stands behind the answer it computed from the unit as assigned"
            );
        }
        other => panic!("expected a settlement, got {other:?}"),
    }
}

#[test]
fn a_result_for_work_that_was_never_assigned_is_refused() {
    // Not an attack so much as a bookkeeping hole: without this, a worker could answer units it
    // was never given, and a quorum could be reached by one machine submitting twice under two
    // names — which the lease does not stop, but which this makes visible rather than silent.
    let (mut grid, unit) = grid_with_one_unit(b"abcde");
    assert_eq!(
        grid.submit_result(unit, answer("nobody", expected(5))),
        Err(Refusal::NotLeased)
    );
}

#[test]
fn one_volunteer_gets_one_vote() {
    let mut grid = Grid::new().with_replication(100);
    let id = grid.register("test", WORKLOAD.as_bytes()).unwrap();
    let unit = grid.submit(&id, b"abcde".to_vec()).unwrap();

    let now = Instant::now();
    grid.lease("alice", now).unwrap();
    grid.submit_result(unit, answer("alice", expected(5)))
        .unwrap();

    assert_eq!(
        grid.submit_result(unit, answer("alice", expected(5))),
        Err(Refusal::AlreadyAnswered),
        "answering twice would let one machine form a quorum by itself"
    );
}

#[test]
fn a_decided_unit_ignores_late_results() {
    let (mut grid, unit) = grid_with_one_unit(b"abcde");
    grid.lease("alice", Instant::now()).unwrap();
    grid.submit_result(unit, answer("alice", expected(5)))
        .unwrap();

    assert_eq!(
        grid.submit_result(unit, answer("bob", expected(5))),
        Err(Refusal::Closed)
    );
}

#[test]
fn a_volunteer_that_vanishes_does_not_hold_the_unit_forever() {
    let mut grid = Grid::new()
        .with_replication(0)
        .with_lease(Duration::from_secs(60));
    let id = grid.register("test", WORKLOAD.as_bytes()).unwrap();
    let unit = grid.submit(&id, b"abcde".to_vec()).unwrap();

    let start = Instant::now();
    assert!(grid.lease("ghost", start).is_some());
    assert!(
        grid.lease("bob", start).is_none(),
        "the unit is leased and the quorum is one, so nobody else is needed yet"
    );

    // Long enough that the ghost's lease has expired.
    let later = start + Duration::from_secs(120);
    let reassigned = grid.lease("bob", later).expect("the lease expired");
    assert_eq!(reassigned.unit, unit);
}

#[test]
fn an_inadmissible_workload_is_refused_at_registration() {
    // The gate runs here, once, before anything is dispatched — so a workload that could never
    // be arbitrated never becomes a work unit in the first place.
    let mut grid = Grid::new();
    let refusal = grid
        .register("bad", br#"(module (func (export "nope")))"#)
        .unwrap_err();
    assert!(
        refusal.contains("not an admissible Cairn workload"),
        "expected the gate's own message: {refusal}"
    );
}

#[test]
fn the_same_workload_registers_as_the_same_unit() {
    // A work unit's identity is the hash of its canonical bytes. Two registrations of the same
    // workload must produce the same id, or a coordinator restarting would orphan every result
    // it had already collected.
    let mut grid = Grid::new();
    let first = grid.register("a", WORKLOAD.as_bytes()).unwrap();
    let second = grid.register("b", WORKLOAD.as_bytes()).unwrap();
    assert_eq!(first, second);

    // And the bytes a volunteer receives carry the fuel counter, so a volunteer can report what
    // the unit cost — see ADR-0009.
    let module = &grid.workload(&first).expect("registered").module;
    assert!(
        module.windows(10).any(|w| w == b"cairn_fuel"),
        "dispatched modules must export the counter"
    );
}

#[test]
fn nothing_is_handed_out_when_there_is_nothing_to_do() {
    let mut grid = Grid::new();
    assert!(grid.lease("alice", Instant::now()).is_none());
}
