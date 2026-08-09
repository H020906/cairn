//! End-to-end checks through the built binary.
//!
//! These run the tool the way a person would, on the example workload, because the properties
//! worth checking here are the ones that only appear when all the pieces are wired together.
//! In particular: `run` and `trace` use **different engines and different instrumentation** and
//! must nevertheless produce the same answer. That is the assumption ADR-0005 rests on, and
//! this is the only place it is checked against a real compiler on a real workload rather than
//! inside the differential harness.

#![allow(clippy::expect_used)]

use std::path::PathBuf;
use std::process::Command;

/// Paths are relative to this crate's root, which is where Cargo runs an integration test.
fn fixture(name: &str) -> PathBuf {
    PathBuf::from("..")
        .join("workloads")
        .join("examples")
        .join(name)
}

fn worker(args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_cairn-worker"))
        .args(args)
        .output()
        .expect("the worker binary should run");
    assert!(
        output.status.success(),
        "cairn-worker {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("output should be UTF-8")
}

/// Pull `key`'s value out of the tool's aligned key/value output.
fn field<'a>(report: &'a str, key: &str) -> &'a str {
    report
        .lines()
        .find_map(|line| line.strip_prefix(key))
        .map(str::trim)
        .unwrap_or_else(|| panic!("no `{key}` in:\n{report}"))
}

#[test]
fn the_two_paths_compute_the_same_answer() {
    let module = fixture("sum-of-squares.wat");
    let module = module.to_str().expect("path should be UTF-8");
    let input = fixture("input-a.bin");
    let input = input.to_str().expect("path should be UTF-8");

    let fast = worker(&["run", module, input]);
    let slow = worker(&["trace", module, input]);

    // The honest path ran on wasmtime with escape-only canonicalization and no metering; the
    // dispute path ran on Cairn's interpreter with everything switched on. Different engines,
    // different bytes, same answer — or a dispute would arbitrate an execution that never
    // happened, against an honest worker.
    assert_eq!(
        field(&fast, "              "),
        field(&slow, "result        "),
        "the honest path and the dispute path disagree about the result"
    );
}

#[test]
fn a_disagreement_is_settled_against_the_assigned_input() {
    let module = fixture("sum-of-squares.wat");
    let module = module.to_str().expect("path should be UTF-8");
    let assigned = fixture("input-a.bin");
    let assigned = assigned.to_str().expect("path should be UTF-8");
    let claimed = fixture("input-b.bin");
    let claimed = claimed.to_str().expect("path should be UTF-8");

    let report = worker(&["dispute", module, assigned, claimed]);

    assert!(
        report.contains("Verdict: the second party was wrong."),
        "expected the party who did not run the assigned unit to lose:\n{report}"
    );

    // The workload reads its input at the very end, so the executions are identical until
    // then. That is deliberate: it is the expensive shape for a dispute, and it exercises the
    // bisection over the full length rather than settling in the first few rounds.
    let divergence: u64 = field(&report, "divergence        step")
        .parse()
        .expect("divergence should be a step number");
    let length: u64 = field(&report, "disputed length   ")
        .trim_end_matches(" instructions")
        .parse()
        .expect("length should be an instruction count");
    assert!(
        divergence > length - 100,
        "expected a late divergence, got {divergence} of {length}"
    );
}

#[test]
fn a_module_that_is_not_admissible_is_refused() {
    // Not a Cairn workload: no `cairn_run` export, and it imports something that does not
    // exist. The tool must reject it before instrumenting rather than fail later in a way that
    // looks like a bug in the runtime.
    let bad = std::env::temp_dir().join("cairn-smoke-not-a-workload.wat");
    std::fs::write(&bad, "(module (func (export \"nope\")))").expect("should write fixture");

    let output = Command::new(env!("CARGO_BIN_EXE_cairn-worker"))
        .args(["run", bad.to_str().expect("path should be UTF-8")])
        .output()
        .expect("the worker binary should run");

    assert!(
        !output.status.success(),
        "an inadmissible module was accepted"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not an admissible Cairn workload"),
        "expected a validation error, got: {stderr}"
    );
}
