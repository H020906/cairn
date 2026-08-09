//! The two metering encodings must be interchangeable.
//!
//! [`Metering::HostCall`] charges with `i32.const N; call $charge`; [`Metering::Global`] charges
//! with `global.get; i64.const N; i64.add; global.set` into a counter the module exports. They
//! exist because they cost wildly different amounts on the two kinds of engine — see
//! [ADR-0009](../../docs/adr/0009-metering-through-a-global-the-engines-disagree.md) — and the
//! whole point is that a work unit may pick either.
//!
//! That only holds if picking is invisible to everything except speed. What must agree:
//!
//! - the **answer**, byte for byte;
//! - the **fuel total**, which is what the network accounts in;
//! - the **fuel value at which execution runs out**, which is a trap and therefore a result;
//! - the **fuel labels of every snapshot**, which is what a bisection walks.
//!
//! What legitimately differs is the **step index**, because the two encodings inject different
//! numbers of instructions. Steps are a private coordinate: both parties to a dispute run the
//! same module, so they agree with each other, which is all bisection needs.
//!
//! One invariant is checked directly rather than through behaviour: the counter global and the
//! interpreter's meter must never drift apart. The global is hashed into every state commitment
//! as an ordinary global while the meter is what decides exhaustion, so a discrepancy would put
//! two honest workers' roots at odds for the same execution.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use cairn_runtime::canon::{self, Canonicalization, Config, Metering};
use cairn_runtime::engine::image;
use cairn_runtime::engine::machine::{Limits, Machine, Trace};
use cairn_runtime::engine::numeric::Trap;
use cairn_runtime::state::Value;
use cairn_runtime::validate;

/// The shapes that exercise the charge sites: a loop, a call tree, and a branch that skips one.
const WORKLOADS: [(&str, &str); 3] = [
    (
        "loop",
        r#"(module
             (import "cairn" "output" (func $output (param i32 i32)))
             (memory (export "memory") 1 4)
             (func (export "cairn_run") (local $i i32) (local $acc i32)
               (block $done
                 (loop $again
                   (br_if $done (i32.ge_u (local.get $i) (i32.const 500)))
                   (local.set $acc (i32.add (i32.mul (local.get $acc) (i32.const 31))
                                            (local.get $i)))
                   (local.set $i (i32.add (local.get $i) (i32.const 1)))
                   (br $again)))
               (i32.store (i32.const 0) (local.get $acc))
               (call $output (i32.const 0) (i32.const 4))))"#,
    ),
    (
        "recursion",
        r#"(module
             (import "cairn" "output" (func $output (param i32 i32)))
             (memory (export "memory") 1 4)
             (func $fib (param $n i32) (result i32)
               (if (result i32) (i32.lt_s (local.get $n) (i32.const 2))
                 (then (local.get $n))
                 (else (i32.add (call $fib (i32.sub (local.get $n) (i32.const 1)))
                                (call $fib (i32.sub (local.get $n) (i32.const 2)))))))
             (func (export "cairn_run")
               (i32.store (i32.const 0) (call $fib (i32.const 12)))
               (call $output (i32.const 0) (i32.const 4))))"#,
    ),
    (
        // Globals of its own, so the counter has to be appended past them without disturbing
        // a single index the workload uses.
        "globals",
        r#"(module
             (import "cairn" "output" (func $output (param i32 i32)))
             (memory (export "memory") 1 4)
             (global $seed (mut i32) (i32.const 12345))
             (global $step i32 (i32.const 7))
             (func (export "cairn_run") (local $i i32)
               (block $done
                 (loop $again
                   (br_if $done (i32.ge_u (local.get $i) (i32.const 200)))
                   (global.set $seed (i32.add (i32.mul (global.get $seed) (i32.const 1103515245))
                                              (global.get $step)))
                   (local.set $i (i32.add (local.get $i) (i32.const 1)))
                   (br $again)))
               (i32.store (i32.const 0) (global.get $seed))
               (call $output (i32.const 0) (i32.const 4))))"#,
    ),
];

fn instrument(text: &str, meter: Metering) -> Vec<u8> {
    let source = wat::parse_str(text).expect("workload should assemble");
    validate::validate_submitted(&source, validate::Limits::default())
        .expect("workload should be a valid Cairn module");
    canon::instrument(
        &source,
        Config {
            meter,
            // Held fixed so any difference is the metering's and nothing else's.
            canonicalize: Canonicalization::Never,
        },
    )
    .expect("instrumentation should succeed")
}

/// Run a workload under one encoding, returning the trace and the final counter global.
fn execute(text: &str, meter: Metering, limits: Limits) -> (Result<Trace, Trap>, Option<i64>) {
    let module = instrument(text, meter);
    let decoded = image::decode(&module).expect("should decode");
    let mut machine = Machine::new(&decoded, Vec::new(), limits).expect("should instantiate");
    let outcome = machine.run();
    let counter = decoded
        .fuel_global
        .and_then(|index| machine.globals().get(index as usize).copied())
        .map(|value| match value {
            Value::I64(n) => n,
            other => panic!("the counter must be an i64, got {other:?}"),
        });
    (outcome, counter)
}

#[test]
fn the_two_encodings_agree_on_everything_but_step_count() {
    for (name, source) in WORKLOADS {
        let (host, host_counter) = execute(source, Metering::HostCall, Limits::default());
        let (global, global_counter) = execute(source, Metering::Global, Limits::default());

        let host = host.expect("workload should not trap");
        let global = global.expect("workload should not trap");

        assert_eq!(host.output, global.output, "{name}: answer");
        assert_eq!(host.fuel, global.fuel, "{name}: fuel total");
        assert_eq!(
            host.snapshots.iter().map(|s| s.fuel).collect::<Vec<_>>(),
            global.snapshots.iter().map(|s| s.fuel).collect::<Vec<_>>(),
            "{name}: snapshot schedule"
        );

        assert_eq!(host_counter, None, "{name}: no counter under HostCall");
        assert_eq!(
            global_counter,
            Some(global.fuel.get() as i64),
            "{name}: the counter must end equal to the meter"
        );

        // Not an incidental difference — it is the cost being traded, and it is exactly
        // measurable. `HostCall` injects two instructions per charge site and `Global` injects
        // four, so the gap between them must equal the gap between `HostCall` and no metering
        // at all. Anything else means one of the encodings charged somewhere the other did not.
        let (bare, _) = execute(source, Metering::Off, Limits::default());
        let bare = bare.expect("workload should not trap");
        assert_eq!(
            global.steps - host.steps,
            host.steps - bare.steps,
            "{name}: two instructions per site against four, at the same sites \
             (bare {}, host {}, global {})",
            bare.steps,
            host.steps,
            global.steps
        );
    }
}

#[test]
fn running_out_of_fuel_happens_at_the_same_fuel_under_both() {
    // Exhaustion is a result: a workload that runs out has *not* produced an answer, and both
    // parties to a dispute must agree on where that happened. It is the one place the two
    // encodings could plausibly diverge, because under `Global` the module has already stored
    // the over-budget total by the time the interpreter sees it.
    for (name, source) in WORKLOADS {
        let limits = Limits {
            fuel: 300,
            ..Limits::default()
        };

        let (host, _) = execute(source, Metering::HostCall, limits);
        let (global, counter) = execute(source, Metering::Global, limits);

        assert_eq!(
            host.map(|t| t.fuel).unwrap_err(),
            Trap::OutOfFuel,
            "{name}: the budget must be small enough to exhaust"
        );
        assert_eq!(global.unwrap_err(), Trap::OutOfFuel, "{name}");

        // And the counter must have been wound back to what was actually spent, rather than
        // left holding the total that was refused.
        let spent = counter.expect("the counter exists under Global");
        assert!(
            spent <= 300,
            "{name}: the counter kept a charge that was never granted: {spent}"
        );
    }
}

#[test]
fn a_workload_cannot_reach_its_own_counter() {
    // The reservation rule, from the outside. `cairn_fuel` is refused at the gate whatever it
    // names, so a submitted module has no way to hold a reference to the counter — and since
    // the counter is appended past the module's own globals, its index is not in the index
    // space the module was validated against either.
    let rejection = validate::validate_submitted(
        &wat::parse_str(
            r#"(module
                 (memory (export "memory") 1 1)
                 (global $g (mut i64) (i64.const 0))
                 (export "cairn_fuel" (global $g))
                 (func (export "cairn_run")))"#,
        )
        .unwrap(),
        validate::Limits::default(),
    )
    .unwrap_err();

    assert_eq!(
        rejection,
        validate::Rejection::ReservedExport {
            name: "cairn_fuel".to_owned()
        }
    );
}
