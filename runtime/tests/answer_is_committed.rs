//! The answer is part of the committed state, and this is what that buys.
//!
//! A state commitment that did not cover what the workload *answered* would let two executions
//! agree at every step and still have returned different results. A coordinator holding two
//! matching traces would have proved nothing about the only thing it cares about, and bisection
//! would correctly report no disagreement while the disagreement sat there.
//!
//! These tests are written to fail if [`StateCommitment::output`] is ever dropped from the root.
//! That is the point: the change it guards is a one-line deletion that no other test in this
//! repository notices.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

use cairn_runtime::canon::{self, Config};
use cairn_runtime::dispute::{Claimant as _, Replay, Step};
use cairn_runtime::engine::image;
use cairn_runtime::engine::machine::{Limits, Machine, Progress};
use cairn_runtime::state;
use cairn_runtime::validate;

/// Assemble, validate and instrument, as a coordinator would.
fn canonical(text: &str) -> Vec<u8> {
    let source = wat::parse_str(text).expect("module should assemble");
    validate::validate_submitted(&source, validate::Limits::default())
        .expect("module should be admissible");
    canon::instrument(&source, Config::dispute_path()).expect("instrumentation should succeed")
}

/// Writes eight bytes into memory and answers with `answered` of them.
///
/// Two builds of this differ in **exactly one immediate**, so their executions run the same
/// instructions, touch the same memory, retire the same fuel and end at the same program
/// counter. The only thing that can differ in the final state is the answer.
fn answering(answered: u32) -> String {
    format!(
        r#"(module
             (import "cairn" "output" (func $output (param i32 i32)))
             (memory (export "memory") 1 1)
             (func (export "cairn_run")
               (i64.store (i32.const 0) (i64.const 0x0807060504030201))
               (call $output (i32.const 0) (i32.const {answered}))))"#
    )
}

fn run(module: &[u8]) -> (cairn_runtime::state::StateCommitment, Vec<u8>) {
    let image = image::decode(module).expect("decodes");
    let mut machine = Machine::new(&image, Vec::new(), Limits::default()).expect("starts");
    let trace = machine.run().expect("runs");
    (machine.commit(), trace.output)
}

#[test]
fn two_executions_that_differ_only_in_their_answer_commit_to_different_roots() {
    // **The load-bearing test for the whole idea.** Delete `output` from `StateCommitment::root`
    // and this is what catches it — nothing else in the suite does, because every other test
    // asks whether the *machine* went the same way rather than whether it *said* the same thing.
    let four = canonical(&answering(4));
    let eight = canonical(&answering(8));

    let (a, answer_a) = run(&four);
    let (b, answer_b) = run(&eight);

    assert_ne!(answer_a, answer_b, "the fixture must produce two answers");

    // Everything a commitment covers *except* the answer is identical. Asserted field by field
    // rather than taken on trust, because if the two executions differed anywhere else the
    // root comparison below would pass for the wrong reason.
    assert_eq!(a.memory, b.memory, "same memory");
    assert_eq!(a.globals, b.globals, "same globals");
    assert_eq!(a.operand_stack, b.operand_stack, "same operand stack");
    assert_eq!(a.call_stack, b.call_stack, "same call stack");
    assert_eq!(a.segments, b.segments, "same dropped segments");
    assert_eq!(a.program_counter, b.program_counter, "same position");
    assert_eq!(a.fuel, b.fuel, "same fuel");

    assert_ne!(
        a.output, b.output,
        "different answers must commit differently"
    );
    assert_ne!(
        a.root(),
        b.root(),
        "two executions that answered differently must not share a state root — otherwise \
         agreeing traces would prove nothing about the answer"
    );
}

#[test]
fn the_committed_digest_is_the_digest_of_the_answer() {
    // The root says *which* answer, not merely *that* they differ. Without this a party could
    // agree on every root and still be holding a different eight bytes.
    let (commitment, answer) = run(&canonical(&answering(8)));
    assert_eq!(commitment.output, state::hash_output(&answer));
    assert_eq!(answer, 0x0807_0605_0403_0201u64.to_le_bytes());
}

#[test]
fn answering_moves_the_root_at_the_instruction_that_answers() {
    // Not merely different at the end: the change lands on the `cairn.output` call itself, which
    // is what makes bisection able to name that instruction as the point of divergence.
    let module = canonical(&answering(8));
    let image = image::decode(&module).unwrap();
    let mut machine = Machine::new(&image, Vec::new(), Limits::default()).unwrap();

    let mut moved_at = None;
    for step in 0..200u64 {
        let before = machine.commit().output;
        let progress = machine.step().unwrap();
        let after = machine.commit().output;
        if before != after {
            moved_at = Some(step);
            break;
        }
        if matches!(progress, Progress::Finished) {
            break;
        }
    }

    assert!(
        moved_at.is_some(),
        "no instruction changed the committed answer, so the answer is not really state"
    );
}

#[test]
fn an_empty_answer_is_a_state_and_not_an_absence() {
    // A workload that answers nothing has answered nothing, which is a fact about its execution
    // and must be committed to. `hash_output(&[])` is a real hash, not a zero.
    let empty = state::hash_output(&[]);
    assert_ne!(empty, [0; 32]);
    assert_ne!(empty, state::hash_output(&[0]));
}

#[test]
fn the_length_of_an_answer_is_part_of_it() {
    // Without a length prefix, an answer's bytes could be reinterpreted — the classic
    // concatenation ambiguity. The four cases that would collide under naive hashing.
    assert_ne!(state::hash_output(b"ab"), state::hash_output(b"a\0b"));
    assert_ne!(state::hash_output(b"a"), state::hash_output(b"a\0"));
    assert_ne!(state::hash_output(b""), state::hash_output(b"\0"));
    assert_eq!(state::hash_output(b"same"), state::hash_output(b"same"));
}

#[test]
fn a_witness_carries_the_answer_as_a_digest_not_as_bytes() {
    // The property that keeps a witness small. A workload answering 64 KiB must still produce
    // witnesses of the same size as one answering four bytes — the digest is 32 bytes either
    // way, and nothing reads the buffer back so nothing needs the contents.
    let big = canonical(
        r#"(module
             (import "cairn" "output" (func $output (param i32 i32)))
             (memory (export "memory") 2 2)
             (func (export "cairn_run")
               (i32.store (i32.const 0) (i32.const 7))
               (call $output (i32.const 0) (i32.const 65536))))"#,
    );
    let image = image::decode(&big).unwrap();
    let mut machine = Machine::new(&image, Vec::new(), Limits::default()).unwrap();

    // Walk to just past the answering call, so the witness describes a state with a large answer
    // already in it.
    let mut widest = 0;
    for _ in 0..200 {
        let witness = machine.witness_for_next_step();
        widest = widest.max(cairn_runtime::wire::encode(&witness).unwrap().len());
        if matches!(machine.step().unwrap(), Progress::Finished) {
            break;
        }
    }

    let mut whole_page = vec![0u8; 65536];
    whole_page[0] = 7;
    assert_eq!(
        machine.commit().output,
        state::hash_output(&whole_page),
        "the fixture should have answered a whole page"
    );

    // Two pages of memory plus proofs is the bulk; a 64 KiB answer adds 32 bytes, not 64 KiB.
    assert!(
        widest < 3 * 65536,
        "a witness grew to {widest} bytes, which means the answer is being carried whole"
    );
}

#[test]
fn a_restored_machine_commits_to_the_answer_it_was_given() {
    // The other half of carrying a digest: an adjudicator rebuilding a machine from a witness
    // has no answer bytes at all, and must still reproduce the same root. If `restore` dropped
    // the digest, every adjudication after the first `cairn.output` call would refuse a
    // perfectly good witness — and refuse it as though the party had fabricated it.
    let module = canonical(&answering(8));
    let image = image::decode(&module).unwrap();
    let mut original = Machine::new(&image, Vec::new(), Limits::default()).unwrap();

    let total = original.clone().run().unwrap().steps;
    for step in 0..total {
        let witness = original.witness_for_next_step();
        assert_eq!(
            witness.commitment().root(),
            original.commit().root(),
            "a witness at step {step} does not describe the state it was taken from"
        );

        let mut expected = original.clone();
        let _ = expected.step().unwrap();

        let mut rebuilt =
            Machine::restore(&image, &witness, Vec::new(), Limits::default()).unwrap();
        let _ = rebuilt.step().unwrap();

        assert_eq!(
            rebuilt.commit().root(),
            expected.commit().root(),
            "a machine rebuilt at step {step} diverged from the original — the answer digest \
             is the field most likely to have been dropped"
        );

        let _ = original.step().unwrap();
    }
}

#[test]
fn a_witness_describes_the_same_state_the_machine_does_at_every_step_including_the_last() {
    // **A witness of the FINAL state is the one nothing used to ask for**, and it was broken.
    // `Machine::commit` reported a finished machine's program counter as the module's entry
    // point; `Witness::commitment` reported zero, because a witness does not know the entry
    // point. For any module whose entry index is not zero the two disagreed — so a party asked
    // for the final state supplied a perfectly good witness and had it refused as fabricated.
    //
    // Found by asking for exactly that witness while settling a dispute the parties agreed
    // about. The range here is `0..=total`, and the `=` is the whole test.
    for entry_padding in 0..3 {
        // Extra functions ahead of the entry point, so `image.entry` is 0, 1 and 2 in turn. With
        // a single-function module the old code was accidentally right.
        let mut filler = String::new();
        for i in 0..entry_padding {
            filler.push_str(&format!("(func $pad{i} (result i32) (i32.const {i}))\n"));
        }
        let module = canonical(&format!(
            r#"(module
                 (import "cairn" "output" (func $output (param i32 i32)))
                 (memory (export "memory") 1 1)
                 {filler}
                 (func (export "cairn_run")
                   (i32.store (i32.const 0) (i32.const 99))
                   (call $output (i32.const 0) (i32.const 4))))"#
        ));
        let image = image::decode(&module).unwrap();

        let mut machine = Machine::new(&image, Vec::new(), Limits::default()).unwrap();
        let total = machine.clone().run().unwrap().steps;

        for step in 0..=total {
            assert_eq!(
                machine.witness_for_next_step().commitment().root(),
                machine.commit().root(),
                "with {entry_padding} functions before the entry point, a witness at step \
                 {step} of {total} describes a different state than the machine it came from"
            );
            if step < total {
                let _ = machine.step().unwrap();
            }
        }
    }
}

#[test]
fn a_replay_reports_the_answer_moving_where_it_moved() {
    // What a party actually answers during a dispute. The roots a `Replay` produces must show
    // the answer changing at the same step the machine did, or the two halves of the protocol
    // are describing different executions.
    let module = canonical(&answering(8));
    let image = image::decode(&module).unwrap();

    let mut probe = Machine::new(&image, Vec::new(), Limits::default()).unwrap();
    let total = probe.run().unwrap().steps;

    let mut party = Replay::new(&image, Vec::new(), Limits::default());
    let mut walker = Machine::new(&image, Vec::new(), Limits::default()).unwrap();
    for step in 0..=total {
        assert_eq!(
            party.root_at(Step::new(step)).unwrap(),
            Some(walker.commit().root()),
            "the replay and the machine disagree at step {step}"
        );
        if step < total {
            let _ = walker.step().unwrap();
        }
    }
}
