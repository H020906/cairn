//! What survives killing the coordinator, and what deliberately does not.
//!
//! Kept beside `grid.rs`'s tests and for the same reason: recovery is a decision about state, and
//! a decision that needed a socket to test would be a decision in the wrong file. What is checked
//! here is the *meaning* of a journal, not the bytes of one — that is `journal.rs`'s own tests,
//! which cover torn tails, damage and unknown tags.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::time::Instant;

use cairn_coordinator::grid::{Grid, Outcome, Submission};
use cairn_coordinator::journal::Entry;

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

fn expected(input_len: u32) -> Vec<u8> {
    input_len.to_le_bytes().to_vec()
}

fn answer(worker: &str, output: Vec<u8>, bisects: bool) -> Submission {
    Submission {
        worker: worker.to_owned(),
        output,
        fuel: None,
        bisects,
    }
}

/// The journal a coordinator would have written for one workload and `inputs.len()` units.
fn queued(inputs: &[&[u8]]) -> Vec<Entry> {
    let mut entries = vec![Entry::Registered {
        name: "test".to_owned(),
        source: WORKLOAD.as_bytes().to_vec(),
    }];
    // The id is not known until the workload is registered, so build a throwaway grid to get it.
    // This is what makes the fixture honest: the journal carries the same id a live coordinator
    // would have written, rather than one the test made up.
    let mut probe = Grid::new();
    let id = probe.register("test", WORKLOAD.as_bytes()).unwrap();
    for input in inputs {
        entries.push(Entry::Queued {
            workload: id.clone(),
            input: (*input).to_vec(),
            quorum: 1,
        });
    }
    entries
}

#[test]
fn a_restarted_coordinator_has_the_same_units_it_died_with() {
    let mut grid = Grid::new();
    let restored = grid
        .restore(&queued(&[b"abcde", b"xy"]))
        .expect("the journal should describe this build");

    assert_eq!(restored.workloads, 1);
    assert_eq!(restored.units, 2);
    assert_eq!(grid.units().len(), 2);
    assert_eq!(grid.unit(0).unwrap().input, b"abcde");
    assert_eq!(grid.unit(1).unwrap().input, b"xy");

    // And they are workable: a volunteer that connects to the restarted coordinator is given the
    // unit the old one never finished.
    let assignment = grid.lease("alice", Instant::now()).expect("work available");
    assert_eq!(assignment.unit, 0);
}

#[test]
fn a_workload_replays_to_the_same_unit_id_it_had() {
    // The journal carries the *submitted* source and replay puts it back through `register`, so
    // instrumentation runs again. That is deliberate: if the instrumentation pass ever changes,
    // this equality breaks loudly instead of the coordinator quietly serving different bytes
    // from the ones its volunteers were given before the restart.
    let mut before = Grid::new();
    let id = before.register("test", WORKLOAD.as_bytes()).unwrap();

    let mut after = Grid::new();
    after.restore(&queued(&[b"abcde"])).unwrap();

    assert!(
        after.workload(&id).is_some(),
        "the restored grid serves a different unit id than the one that died"
    );
}

#[test]
fn work_that_was_finished_is_not_handed_out_again() {
    // "Nothing repeated" — the other half of the promise. An accepted unit must not come back as
    // available, or a restart would quietly re-do every unit the grid had ever completed.
    let mut entries = queued(&[b"abcde", b"xy"]);
    entries.push(Entry::Answered {
        unit: 0,
        worker: "alice".to_owned(),
        output: expected(5),
        fuel: Some(42),
        bisects: false,
    });
    entries.push(Entry::Accepted {
        unit: 0,
        output: expected(5),
    });

    let mut grid = Grid::new();
    let restored = grid.restore(&entries).unwrap();
    assert_eq!(restored.results, 1);
    assert_eq!(restored.decided, 1);

    assert_eq!(
        grid.unit(0).unwrap().outcome,
        Outcome::Accepted {
            output: expected(5)
        }
    );
    let assignment = grid.lease("bob", Instant::now()).expect("unit 1 is open");
    assert_eq!(assignment.unit, 1, "a finished unit was handed out again");
}

#[test]
fn a_result_from_a_worker_whose_lease_died_with_the_coordinator_is_still_accepted() {
    // **This test is why leases are journalled at all.** The first draft skipped them, on the
    // reasoning that every lease has expired by the time a restart happens anyway — and this
    // failed with `NotLeased`: the volunteer that was mid-unit comes back with a good answer and
    // is turned away, because the only evidence it was ever given the work died with the process.
    // It did the work. The answer is good. Throwing it away is exactly the loss this feature
    // exists to prevent.
    let mut entries = queued(&[b"abcde"]);
    entries.push(Entry::Leased {
        unit: 0,
        worker: "alice".to_owned(),
    });

    let mut grid = Grid::new();
    let restored = grid.restore(&entries).unwrap();
    assert_eq!(restored.leases, 1);

    // The unit is available to everybody else *at the same time*: the restored lease is evidence
    // that alice was assigned it, not a reservation holding it for a volunteer who may be gone.
    assert!(
        grid.lease("bob", Instant::now()).is_some(),
        "a restored lease reserved the unit instead of merely remembering it"
    );

    let outcome = grid
        .submit_result(0, answer("alice", expected(5), false))
        .expect("a result for an open unit is accepted");

    assert_eq!(
        outcome,
        Outcome::Accepted {
            output: expected(5)
        },
        "a volunteer was punished for the coordinator restarting under it"
    );
}

#[test]
fn a_unit_that_was_mid_argument_comes_back_unassigned_and_convicts_nobody() {
    // **The load-bearing decision in the whole feature.** A dispute is a live protocol with a
    // blocking referee, two mailboxes and two volunteers mid-replay; it cannot be rebuilt from a
    // file. The alternative to voiding it is resuming and timing out whichever party did not
    // come back — which convicts an honest volunteer for the coordinator's crash, and that is
    // the worst outcome this project has.
    let mut entries = queued(&[b"abcde"]);
    entries.push(Entry::Answered {
        unit: 0,
        worker: "honest".to_owned(),
        output: expected(5),
        fuel: None,
        bisects: true,
    });
    entries.push(Entry::Answered {
        unit: 0,
        worker: "liar".to_owned(),
        output: vec![0xde, 0xad],
        fuel: None,
        bisects: true,
    });
    entries.push(Entry::Disputed {
        unit: 0,
        parties: ["honest".to_owned(), "liar".to_owned()],
    });

    let mut grid = Grid::new();
    let restored = grid.restore(&entries).unwrap();

    assert_eq!(
        restored.voided,
        vec![(0, ["honest".to_owned(), "liar".to_owned()])],
        "the restart must say whose argument it dropped"
    );
    assert_eq!(grid.unit(0).unwrap().outcome, Outcome::Open);
    assert!(
        grid.unit(0).unwrap().results.is_empty(),
        "a voided unit that kept its results would never be handed out again — `lease` counts \
         results against the quorum, so it would sit there looking available forever"
    );
    assert!(
        grid.disputes().is_empty(),
        "restoring must not start an argument against volunteers who are not connected"
    );

    // Both parties are eligible again. Neither did anything wrong, and refusing the honest one
    // would be a penalty for having been in an argument the coordinator abandoned.
    assert!(grid.lease("honest", Instant::now()).is_some());
    let mut grid = Grid::new();
    grid.restore(&entries).unwrap();
    assert!(grid.lease("liar", Instant::now()).is_some());
}

#[test]
fn a_units_quorum_survives_a_restart_with_a_different_replication_rate() {
    // The quorum is journalled rather than recomputed. `submit` derives it from the unit index
    // and the replication percentage, so a coordinator restarted with a different `--replicate`
    // would otherwise change the quorum of units volunteers are already working on — turning a
    // spot-checked unit into an unchecked one, or stranding an accepted one.
    let entries: Vec<Entry> = queued(&[b"abcde"])
        .into_iter()
        .map(|entry| match entry {
            Entry::Queued {
                workload, input, ..
            } => Entry::Queued {
                workload,
                input,
                quorum: 2,
            },
            other => other,
        })
        .collect();

    let mut grid = Grid::new().with_replication(0);
    grid.restore(&entries).unwrap();

    assert_eq!(
        grid.unit(0).unwrap().quorum,
        2,
        "the quorum was recomputed from the current flag instead of read from the journal"
    );
}

#[test]
fn a_journal_that_does_not_describe_this_build_stops_the_coordinator() {
    // Coming up with a grid that is *nearly* the one that died is worse than not coming up. A
    // unit index that does not exist means the file and the binary disagree about what happened,
    // and every decision made afterwards would rest on that disagreement.
    let mut entries = queued(&[b"abcde"]);
    entries.push(Entry::Accepted {
        unit: 7,
        output: expected(5),
    });

    let mut grid = Grid::new();
    let refused = grid.restore(&entries);
    assert!(refused.is_err(), "a journal naming unit 7 was accepted");
    assert!(refused.unwrap_err().contains("no unit 7"));
}

#[test]
fn a_settled_verdict_survives_but_a_disputed_one_does_not() {
    // Two disagreements, two fates, and the asymmetry is the point. A dispute settled by the
    // referee re-executing the unit is *finished* — there is a verdict and an answer, and they
    // are worth keeping. A dispute still being argued is not, and pretending otherwise would
    // mean resuming it.
    let mut entries = queued(&[b"abcde", b"xy"]);
    entries.push(Entry::Settled {
        unit: 0,
        verdict: "the second party was wrong".to_owned(),
        output: Some(expected(5)),
    });
    entries.push(Entry::Disputed {
        unit: 1,
        parties: ["honest".to_owned(), "liar".to_owned()],
    });

    let mut grid = Grid::new();
    grid.restore(&entries).unwrap();

    assert_eq!(
        grid.unit(0).unwrap().outcome,
        Outcome::Settled {
            verdict: "the second party was wrong".to_owned(),
            output: Some(expected(5)),
        }
    );
    assert_eq!(grid.unit(1).unwrap().outcome, Outcome::Open);
}
