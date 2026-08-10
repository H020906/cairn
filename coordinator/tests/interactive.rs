//! The interactive dispute protocol, driven end to end without a socket.
//!
//! This is the file that checks the project's central claim actually happens in the product:
//! that two volunteers who disagree about an execution of *n* instructions settle it in about
//! `log₂(n)` messages and one executed instruction, rather than by anybody re-running the work.
//!
//! The volunteers here are threads polling a [`Desk`] — the same mailbox the HTTP handlers poll,
//! reached through the same public methods. What is *not* exercised is the HTTP translation
//! itself, which is deliberate: `api.rs` decides nothing, and a test that needed a port to prove
//! a decision would mean the decision was in the wrong file.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use cairn_coordinator::dispute::{answer_honestly, Answer, Conclusion, Desk, Question};
use cairn_coordinator::grid::{Grid, Outcome, Submission};
use cairn_runtime::dispute::Party;
use cairn_runtime::engine::image;
use cairn_runtime::engine::machine::Limits;

/// Long enough to bisect over — four hundred iterations of a loop, a few thousand instructions.
///
/// A shorter workload would settle in three or four rounds, which proves the mechanism runs but
/// says nothing about whether it *scales*, and scaling is the entire argument.
const LOOPING: &str = r#"
    (module
      (import "cairn" "input"  (func $input  (param i32 i32) (result i32)))
      (import "cairn" "output" (func $output (param i32 i32)))
      (memory (export "memory") 1 1)
      (func (export "cairn_run") (local $i i32) (local $sum i32)
        (local.set $sum (call $input (i32.const 0) (i32.const 0)))
        (block $done
          (loop $again
            (br_if $done (i32.ge_u (local.get $i) (i32.const 400)))
            (local.set $sum (i32.add (local.get $sum) (local.get $i)))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $again)))
        (i32.store (i32.const 0) (local.get $sum))
        (call $output (i32.const 0) (i32.const 4))))
"#;

/// Sum of 0..400 plus the input's length, which is what the workload writes.
fn expected(input_len: u32) -> Vec<u8> {
    (399 * 400 / 2 + input_len).to_le_bytes().to_vec()
}

fn arguing(worker: &str, output: Vec<u8>) -> Submission {
    Submission {
        worker: worker.to_owned(),
        output,
        fuel: None,
        bisects: true,
    }
}

/// A grid holding one replicated unit, plus the bytes a party has to replay.
///
/// The dispute-path module, not the one a volunteer runs. They are different programs with
/// different instruction counts, and "step 2,000" names a state only if both parties replay the
/// same bytes.
fn ready(input: &[u8], patience: Duration) -> (Arc<Mutex<Grid>>, usize, Arc<Vec<u8>>) {
    let mut grid = Grid::new().with_replication(100).with_patience(patience);
    let id = grid
        .register("looping", LOOPING.as_bytes())
        .expect("admissible");
    let unit = grid.submit(&id, input.to_vec()).expect("queued");
    let disputable = Arc::clone(&grid.workload(&id).expect("registered").disputable);

    let now = Instant::now();
    grid.lease("first", now).expect("work");
    grid.lease("second", now).expect("work");
    (Arc::new(Mutex::new(grid)), unit, disputable)
}

/// A volunteer answering challenges, honestly or otherwise.
///
/// `lies_from` corrupts every root at or after that step, which is what a party defending a
/// result it did not compute has to do: an honest replay would reproduce the truth and agree.
fn volunteer(
    grid: &Arc<Mutex<Grid>>,
    name: &str,
    module: Arc<Vec<u8>>,
    input: Vec<u8>,
    lies_from: Option<u64>,
) -> thread::JoinHandle<u32> {
    let grid = Arc::clone(grid);
    let name = name.to_owned();
    thread::spawn(move || {
        // Decoded once. A party answers `log₂(n)` questions and decoding per answer would be
        // measurable next to the replays themselves.
        let image = image::decode(&module).expect("the disputed module decodes");
        let mut answered = 0;
        // A party polls faster than the referee advances, so it will see a question it has
        // already answered. Answering it again is harmless but wasteful — each answer costs an
        // interpreted replay — so a real worker remembers the last token it dealt with.
        let mut done: Option<u64> = None;
        let deadline = Instant::now() + Duration::from_secs(120);
        while Instant::now() < deadline {
            // The whole client side: find my desk, see if anything is outstanding, answer it.
            let outstanding = {
                let grid = grid.lock().unwrap();
                grid.dispute_for(&name).and_then(|(_, dispute)| {
                    let desk: Arc<Desk> = Arc::clone(dispute.desk_for(&name)?);
                    let (token, question) = desk.pending()?;
                    (done != Some(token)).then_some((desk, token, question))
                })
            };

            let Some((desk, token, question)) = outstanding else {
                // Either no dispute involves this worker yet, or it has finished. Both look the
                // same from here, which is why a volunteer needs no state beyond its name.
                if grid
                    .lock()
                    .unwrap()
                    .disputes()
                    .iter()
                    .any(|d| d.is_finished() && d.desk_for(&name).is_some())
                {
                    break;
                }
                thread::sleep(Duration::from_millis(2));
                continue;
            };

            let mut answer = answer_honestly(&image, &input, Limits::default(), question)
                .expect("a party can answer");
            if let (Some(from), Question::Root { step }, Answer::Root(root)) =
                (lies_from, question, &answer)
            {
                if step >= from {
                    answer = Answer::Root(root.map(|mut r| {
                        r[0] ^= 0xff;
                        r
                    }));
                }
            }

            // Every answer in every interactive test also checks the token rule, rather than one
            // test checking it once. A party that polls, goes away for a while and comes back
            // must not have its stale answer counted as a reply to whatever is outstanding now —
            // that is an answer to a question it was never asked.
            //
            // The probe is the *previous* token, deliberately. An earlier version used
            // `token + 1` and it failed, correctly: tokens only ever increase, so a future token
            // is one the referee is about to issue, and a probe that guesses it lands on a real
            // question. Only the past is safely invalid.
            assert!(
                !desk.reply(token.wrapping_sub(1), Answer::Length(0)),
                "an answer quoting a stale token must be refused"
            );

            if desk.reply(token, answer) {
                answered += 1;
                done = Some(token);
            }
        }
        answered
    })
}

/// Block until the dispute concludes, and hand back what it concluded.
fn await_conclusion(
    grid: &Arc<Mutex<Grid>>,
    dispute: usize,
) -> (Conclusion, Option<Vec<u8>>, usize) {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        {
            let grid = grid.lock().unwrap();
            let argument = grid.dispute(dispute).expect("the dispute exists");
            let log = argument.log();
            if let Some(conclusion) = log.conclusion.clone() {
                return (conclusion, log.output.clone(), log.transcript.len());
            }
        }
        assert!(Instant::now() < deadline, "the dispute never concluded");
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_liar_is_convicted_by_bisection_and_one_executed_instruction() {
    // The claim, exercised. Nobody re-runs the unit: the parties replay on their own machines,
    // the referee exchanges a few dozen messages and executes exactly one instruction.
    let input = b"twelve chars";
    let (grid, unit, module) = ready(input, Duration::from_secs(30));

    let honest = volunteer(&grid, "first", Arc::clone(&module), input.to_vec(), None);
    let liar = volunteer(
        &grid,
        "second",
        Arc::clone(&module),
        input.to_vec(),
        Some(900),
    );

    let dispute = {
        let mut grid = grid.lock().unwrap();
        grid.submit_result(unit, arguing("first", expected(input.len() as u32)))
            .expect("first result");
        match grid
            .submit_result(unit, arguing("second", vec![0xde, 0xad, 0xbe, 0xef]))
            .expect("second result")
        {
            Outcome::Disputed { dispute } => dispute,
            other => panic!("two arguing parties must produce a dispute, got {other:?}"),
        }
    };

    let (conclusion, output, messages) = await_conclusion(&grid, dispute);

    match conclusion {
        Conclusion::Convicted {
            liar,
            divergence,
            rounds,
        } => {
            assert_eq!(liar, Party::Second, "the liar answered second");
            assert_eq!(
                divergence, 899,
                "the instruction running from step 899 to 900 is where the lie starts"
            );
            // The economic claim, as an assertion rather than a paragraph. The execution is
            // thousands of instructions long; a dozen questions settled it.
            assert!(
                rounds <= 16,
                "a few thousand instructions should bisect in ~12 rounds, took {rounds}"
            );
        }
        other => panic!("expected a conviction, got {other:?}"),
    }

    assert_eq!(
        output,
        Some(expected(input.len() as u32)),
        "convicting one party accepts the other's answer, with no execution of the unit anywhere"
    );
    // Two parties, one question each per round, plus the opening and closing exchanges.
    assert!(messages < 40, "the transcript ran to {messages} messages");

    assert!(honest.join().unwrap() > 0);
    assert!(liar.join().unwrap() > 0);
}

#[test]
fn the_message_count_grows_with_the_logarithm_of_the_execution() {
    // One data point is a demonstration; two an order apart is the claim. A dispute over an
    // execution eight times longer must cost about three more rounds, not eight times as many.
    let mut rounds_seen = Vec::new();

    for (iterations, lie_at) in [(50u32, 100u64), (400, 900)] {
        let source = LOOPING.replace("(i32.const 400)", &format!("(i32.const {iterations})"));
        let input = b"x";

        let mut grid = Grid::new()
            .with_replication(100)
            .with_patience(Duration::from_secs(30));
        let id = grid.register("looping", source.as_bytes()).unwrap();
        let unit = grid.submit(&id, input.to_vec()).unwrap();
        let module = Arc::clone(&grid.workload(&id).unwrap().disputable);
        let now = Instant::now();
        grid.lease("first", now).unwrap();
        grid.lease("second", now).unwrap();
        let grid = Arc::new(Mutex::new(grid));

        let a = volunteer(&grid, "first", Arc::clone(&module), input.to_vec(), None);
        let b = volunteer(
            &grid,
            "second",
            Arc::clone(&module),
            input.to_vec(),
            Some(lie_at),
        );

        let dispute = {
            let mut grid = grid.lock().unwrap();
            grid.submit_result(unit, arguing("first", vec![1, 2, 3, 4]))
                .unwrap();
            match grid
                .submit_result(unit, arguing("second", vec![4, 3, 2, 1]))
                .unwrap()
            {
                Outcome::Disputed { dispute } => dispute,
                other => panic!("expected a dispute, got {other:?}"),
            }
        };

        let (conclusion, _, _) = await_conclusion(&grid, dispute);
        rounds_seen.push((iterations, conclusion.rounds()));
        a.join().unwrap();
        b.join().unwrap();
    }

    let (short_len, short_rounds) = rounds_seen[0];
    let (long_len, long_rounds) = rounds_seen[1];
    assert!(
        long_rounds < short_rounds + 6,
        "an execution {}× longer took {long_rounds} rounds against {short_rounds} — that is not \
         logarithmic",
        long_len / short_len
    );
}

#[test]
fn a_party_that_stops_answering_loses_by_default() {
    // Volunteers close laptops. This must be a *different* outcome from a proven lie, because
    // ADR-0001 wants it to cost the volunteer very differently.
    let input = b"gone";
    let (grid, unit, module) = ready(input, Duration::from_secs(3));

    // Only one party plays. The other never polls.
    let present = volunteer(&grid, "first", Arc::clone(&module), input.to_vec(), None);

    let dispute = {
        let mut grid = grid.lock().unwrap();
        grid.submit_result(unit, arguing("first", expected(4)))
            .unwrap();
        match grid
            .submit_result(unit, arguing("second", vec![0; 4]))
            .unwrap()
        {
            Outcome::Disputed { dispute } => dispute,
            other => panic!("expected a dispute, got {other:?}"),
        }
    };

    let (conclusion, output, _) = await_conclusion(&grid, dispute);
    match conclusion {
        Conclusion::Abandoned { by, .. } => assert_eq!(by, Party::Second),
        other => panic!("expected an abandonment, got {other:?}"),
    }
    assert_eq!(
        output,
        Some(expected(4)),
        "the party still answering is believed, which is what losing by default means"
    );
    present.join().unwrap();
}

#[test]
fn two_honest_parties_that_disagree_are_settled_without_executing_anything() {
    // **The non-adversarial case, and it used to be the most expensive path in the system.**
    // Both parties replay the same bytes under the same deterministic interpreter, so both
    // reproduce the truth and bisection finds nothing to convict — nobody lied, one of them was
    // merely wrong. Naming the wrong answer used to cost the coordinator a full interpreted
    // re-execution.
    //
    // It no longer does, because the answer is part of the committed state: the trace they agree
    // on *determines* what the answer was, so one witness of the final state plus two hash
    // comparisons settles it. Nothing is executed — not the unit, and not even one instruction.
    let input = b"honest";
    let (grid, unit, module) = ready(input, Duration::from_secs(30));

    let a = volunteer(&grid, "first", Arc::clone(&module), input.to_vec(), None);
    let b = volunteer(&grid, "second", Arc::clone(&module), input.to_vec(), None);

    let dispute = {
        let mut grid = grid.lock().unwrap();
        grid.submit_result(unit, arguing("first", expected(6)))
            .unwrap();
        // A wrong answer from a party that will nonetheless replay honestly: a broken engine, a
        // miscompiled build, cosmic rays. Not a liar.
        match grid
            .submit_result(unit, arguing("second", vec![7; 4]))
            .unwrap()
        {
            Outcome::Disputed { dispute } => dispute,
            other => panic!("expected a dispute, got {other:?}"),
        }
    };

    let (conclusion, output, _) = await_conclusion(&grid, dispute);
    match conclusion {
        Conclusion::AgreedOnTrace { wrong, .. } => assert_eq!(
            wrong,
            Some(Party::Second),
            "the trace both parties agreed on names the party whose answer contradicted it"
        ),
        Conclusion::FellBack { why, verdict } => {
            panic!("this should no longer need re-execution: {why} / {verdict}")
        }
        other => panic!("expected agreement on the trace, got {other:?}"),
    }
    assert_eq!(output, Some(expected(6)));
    a.join().unwrap();
    b.join().unwrap();
}

#[test]
fn a_fabricated_witness_does_not_decide_a_dispute() {
    // The check that makes it safe to take the disputed state from an interested party. A liar
    // asked for the state it has just been caught misreporting will hand over whatever helps it;
    // the referee refuses anything that does not reconstruct the root *both* parties committed
    // to, and asks the other side instead.
    let input = b"forge";
    let (grid, unit, module) = ready(input, Duration::from_secs(30));

    let honest = volunteer(&grid, "first", Arc::clone(&module), input.to_vec(), None);

    // A party that answers roots dishonestly from step 600 and then supplies a witness that is
    // real but from the wrong state — the subtlest forgery available to it, since every proof in
    // it verifies.
    let forger = {
        let grid = Arc::clone(&grid);
        let module = Arc::clone(&module);
        let input = input.to_vec();
        thread::spawn(move || {
            let image = image::decode(&module).expect("the disputed module decodes");
            let mut done: Option<u64> = None;
            let deadline = Instant::now() + Duration::from_secs(120);
            while Instant::now() < deadline {
                let outstanding = {
                    let grid = grid.lock().unwrap();
                    grid.dispute_for("second").and_then(|(_, d)| {
                        let desk = Arc::clone(d.desk_for("second")?);
                        let (token, question) = desk.pending()?;
                        (done != Some(token)).then_some((desk, token, question))
                    })
                };
                let Some((desk, token, question)) = outstanding else {
                    if grid
                        .lock()
                        .unwrap()
                        .disputes()
                        .iter()
                        .any(|d| d.is_finished() && d.desk_for("second").is_some())
                    {
                        break;
                    }
                    thread::sleep(Duration::from_millis(2));
                    continue;
                };

                let answer = match question {
                    Question::Root { step } if step >= 600 => {
                        let Answer::Root(root) =
                            answer_honestly(&image, &input, Limits::default(), question).unwrap()
                        else {
                            unreachable!("a root question is answered with a root")
                        };
                        Answer::Root(root.map(|mut r| {
                            r[0] ^= 0xff;
                            r
                        }))
                    }
                    // Handing over a genuine state from one step later. Every proof in it holds;
                    // it simply is not the state under dispute.
                    Question::Witness { step } => answer_honestly(
                        &image,
                        &input,
                        Limits::default(),
                        Question::Witness { step: step + 1 },
                    )
                    .unwrap(),
                    other => answer_honestly(&image, &input, Limits::default(), other).unwrap(),
                };
                if desk.reply(token, answer) {
                    done = Some(token);
                }
            }
        })
    };

    let dispute = {
        let mut grid = grid.lock().unwrap();
        grid.submit_result(unit, arguing("first", expected(5)))
            .unwrap();
        match grid
            .submit_result(unit, arguing("second", vec![6; 4]))
            .unwrap()
        {
            Outcome::Disputed { dispute } => dispute,
            other => panic!("expected a dispute, got {other:?}"),
        }
    };

    let (conclusion, output, _) = await_conclusion(&grid, dispute);
    assert_eq!(
        conclusion,
        Conclusion::Convicted {
            liar: Party::Second,
            divergence: 599,
            rounds: conclusion.rounds(),
        },
        "the forged witness was refused and the honest party's supplied instead"
    );
    assert_eq!(output, Some(expected(5)));
    honest.join().unwrap();
    forger.join().unwrap();
}

#[test]
fn being_in_a_dispute_is_distinguishable_from_having_nothing_to_do() {
    // **The distinction a party's survival depends on.** The referee asks one side at a time, so
    // a party spends most of a dispute with nothing outstanding. If that is indistinguishable
    // from "you are not in a dispute", a worker with any idle timeout eventually goes home — and
    // going home during a dispute means *losing by default*, so it would be convicted for the
    // other party being slow.
    //
    // Found by running it: a native volunteer with `--idle-exit 60` abandoned a dispute it was
    // winning, and the coordinator recorded "the second party stopped answering". The fix is
    // that `dispute_for` answers a different question from `pending`, and `/api/challenge`
    // reports three states rather than two.
    // No volunteers at all, so the state is deterministic rather than raced for: the referee's
    // first act is to ask the *first* party how long its execution was, and it blocks there. The
    // second party is therefore squarely in a dispute with nothing outstanding — which is the
    // state under test — from the moment the dispute opens.
    let input = b"turns";
    let (grid, unit, _) = ready(input, Duration::from_millis(300));

    let dispute = {
        let mut grid = grid.lock().unwrap();
        grid.submit_result(unit, arguing("first", expected(5)))
            .unwrap();
        match grid
            .submit_result(unit, arguing("second", vec![3; 4]))
            .unwrap()
        {
            Outcome::Disputed { dispute } => dispute,
            other => panic!("expected a dispute, got {other:?}"),
        }
    };

    {
        let grid = grid.lock().unwrap();
        let (_, argument) = grid
            .dispute_for("second")
            .expect("a party is in a dispute from the moment it opens");
        assert!(
            argument
                .desk_for("second")
                .expect("a party has a desk")
                .pending()
                .is_none(),
            "the referee asks one side at a time; the other must have nothing outstanding"
        );

        // And somebody who is not a party is not in a dispute at all. Different answer, different
        // meaning, and a worker's survival depends on acting on the difference.
        assert!(grid.dispute_for("a-stranger").is_none());
    }

    // Neither party ever answers, so this falls back — which is correct and not the point here.
    let (_, _, _) = await_conclusion(&grid, dispute);
}

#[test]
fn an_unprompted_answer_is_refused() {
    // A party volunteering a root nobody asked for must change nothing. The token rule proper is
    // checked on every answer of every test above, in `volunteer`.
    let desk = Desk::new();
    assert!(desk.pending().is_none(), "an idle desk asks nothing");
    for token in [0u64, 1, u64::MAX] {
        assert!(
            !desk.reply(token, Answer::Root(None)),
            "an answer to nothing was accepted with token {token}"
        );
    }
}

#[test]
fn a_volunteer_that_cannot_argue_is_never_challenged() {
    // The rule that keeps an honest browser out of a protocol it cannot take part in. Two
    // non-arguing parties disagreeing must produce a settlement and *no dispute at all* — not a
    // dispute that later times both of them out.
    let input = b"browser";
    let mut grid = Grid::new().with_replication(100);
    let id = grid.register("looping", LOOPING.as_bytes()).unwrap();
    let unit = grid.submit(&id, input.to_vec()).unwrap();
    let now = Instant::now();
    grid.lease("chrome", now).unwrap();
    grid.lease("firefox", now).unwrap();

    let blind = |worker: &str, output: Vec<u8>| Submission {
        worker: worker.to_owned(),
        output,
        fuel: None,
        bisects: false,
    };

    grid.submit_result(unit, blind("chrome", expected(7)))
        .unwrap();
    match grid
        .submit_result(unit, blind("firefox", vec![0; 4]))
        .unwrap()
    {
        Outcome::Settled { verdict, output } => {
            assert!(verdict.contains("second party"), "{verdict}");
            assert!(
                verdict.contains("re-execution"),
                "the route must be stated: {verdict}"
            );
            assert_eq!(output, Some(expected(7)));
        }
        other => panic!("expected the fallback, got {other:?}"),
    }
    assert!(
        grid.disputes().is_empty(),
        "challenging a party that cannot answer would convict it for silence"
    );
    assert!(grid.dispute_for("chrome").is_none());
}

#[test]
fn one_arguing_party_is_not_enough() {
    // Half a protocol is not a protocol: bisection needs *both* sides to answer. A grid that
    // opened a dispute here would time the blind party out and convict it.
    let input = b"mixed";
    let mut grid = Grid::new().with_replication(100);
    let id = grid.register("looping", LOOPING.as_bytes()).unwrap();
    let unit = grid.submit(&id, input.to_vec()).unwrap();
    let now = Instant::now();
    grid.lease("native", now).unwrap();
    grid.lease("chrome", now).unwrap();

    grid.submit_result(unit, arguing("native", expected(5)))
        .unwrap();
    let outcome = grid
        .submit_result(
            unit,
            Submission {
                worker: "chrome".to_owned(),
                output: vec![0; 4],
                fuel: None,
                bisects: false,
            },
        )
        .unwrap();

    assert!(
        matches!(outcome, Outcome::Settled { .. }),
        "expected the fallback, got {outcome:?}"
    );
    assert!(grid.disputes().is_empty());
}
