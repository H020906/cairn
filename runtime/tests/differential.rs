//! Differential testing: Cairn's interpreter against an independent WebAssembly engine.
//!
//! # Why this file is the most important test in the project
//!
//! Cairn settles disputes by finding the first instruction at which two workers diverged. The
//! scheme is sound only while two *honest* engines running the same instrumented module agree
//! exactly. If they can disagree, arbitration does not catch cheats — it convicts whichever
//! honest volunteer happened to be running the engine that lost.
//!
//! That failure would be silent, rare, and concentrated on unusual hardware, which is the
//! worst combination a bug can have. So the interpreter is not tested only against expected
//! answers written by the same person who wrote it. It is tested against
//! [`wasmi`](https://docs.rs/wasmi), a mature engine developed independently, on the same
//! bytes.
//!
//! # What is compared
//!
//! - **Fuel.** The strongest signal here. Both engines drive the same injected `cairn.charge`
//!   calls, so identical totals mean they took the same path through the same number of
//!   instructions. A control-flow divergence shows up as a fuel mismatch even when the two
//!   happen to produce the same answer.
//! - **Output bytes**, when execution completes.
//! - **Whether execution trapped.** The two engines classify traps differently, so the
//!   comparison is on the fact of trapping rather than on a shared taxonomy; the trap kinds
//!   are checked against expectations in the unit tests instead.

// The crate denies these because a panic inside the execution kernel would corrupt a trace
// rather than fail loudly. That reasoning does not carry into a test harness: here a failed
// `expect` means the harness itself is broken, and saying so immediately is the correct
// behaviour. Only the harness scaffolding uses them; the comparisons themselves are
// assertions.
#![allow(clippy::expect_used, clippy::indexing_slicing)]

use cairn_runtime::canon::{self, Config};
use cairn_runtime::engine::image;
use cairn_runtime::engine::machine::{Limits, Machine};
use cairn_runtime::validate;

/// What an engine did with a work unit, reduced to what both can report.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Outcome {
    /// `None` when execution trapped.
    output: Option<Vec<u8>>,
    /// Instructions charged. Meaningful even on a trap: it says how far execution got.
    fuel: u64,
}

/// Assemble, validate and instrument, exactly as a coordinator would.
fn canonical(text: &str) -> Vec<u8> {
    let source = wat::parse_str(text).expect("module should assemble");
    validate::validate_submitted(&source, validate::Limits::default())
        .expect("module should be a valid Cairn workload");
    canon::instrument(&source, Config::default()).expect("instrumentation should succeed")
}

/// Run under Cairn's interpreter.
fn run_cairn(module: &[u8], input: &[u8]) -> Outcome {
    let image = image::decode(module).expect("instrumented module should decode");
    let mut machine = match Machine::new(&image, input.to_vec(), Limits::default()) {
        Ok(machine) => machine,
        Err(_) => {
            return Outcome {
                output: None,
                fuel: 0,
            }
        }
    };
    match machine.run() {
        Ok(trace) => Outcome {
            output: Some(trace.output),
            fuel: trace.fuel.get(),
        },
        Err(_) => Outcome {
            output: None,
            fuel: machine.fuel().get(),
        },
    }
}

/// Host state for the reference engine, mirroring what the machine keeps internally.
#[derive(Default)]
struct Host {
    input: Vec<u8>,
    output: Vec<u8>,
    fuel: u64,
}

/// Run under wasmi.
fn run_wasmi(module: &[u8], input: &[u8]) -> Outcome {
    use wasmi::{Caller, Engine, Extern, Linker, Module, Store};

    let engine = Engine::default();
    let module = Module::new(&engine, module).expect("reference engine should accept the module");
    let mut store = Store::new(
        &engine,
        Host {
            input: input.to_vec(),
            ..Host::default()
        },
    );
    let mut linker = <Linker<Host>>::new(&engine);

    linker
        .func_wrap(
            "cairn",
            "charge",
            |mut caller: Caller<'_, Host>, instructions: i32| {
                caller.data_mut().fuel += u64::from(instructions as u32);
            },
        )
        .expect("charge should link");

    linker
        .func_wrap(
            "cairn",
            "input",
            |mut caller: Caller<'_, Host>, ptr: i32, len: i32| -> i32 {
                let available = caller.data().input.len();
                let count = available.min(len as u32 as usize);
                if count > 0 {
                    let bytes = caller.data().input[..count].to_vec();
                    let memory = caller
                        .get_export("memory")
                        .and_then(Extern::into_memory)
                        .expect("workload exports its memory");
                    // A failed write means the workload asked for an out-of-bounds address;
                    // wasmi will trap on its own shortly, and Cairn traps immediately.
                    let _ = memory.write(&mut caller, ptr as u32 as usize, &bytes);
                }
                available as i32
            },
        )
        .expect("input should link");

    linker
        .func_wrap(
            "cairn",
            "output",
            |mut caller: Caller<'_, Host>, ptr: i32, len: i32| {
                let mut buffer = vec![0u8; len as u32 as usize];
                let memory = caller
                    .get_export("memory")
                    .and_then(Extern::into_memory)
                    .expect("workload exports its memory");
                if memory
                    .read(&caller, ptr as u32 as usize, &mut buffer)
                    .is_ok()
                {
                    caller.data_mut().output = buffer;
                }
            },
        )
        .expect("output should link");

    let Ok(instance) = linker.instantiate_and_start(&mut store, &module) else {
        return Outcome {
            output: None,
            fuel: store.data().fuel,
        };
    };

    let entry = instance
        .get_typed_func::<(), ()>(&store, validate::ENTRY_POINT)
        .expect("workload exports its entry point");

    match entry.call(&mut store, ()) {
        Ok(()) => Outcome {
            output: Some(store.data().output.clone()),
            fuel: store.data().fuel,
        },
        Err(_) => Outcome {
            output: None,
            fuel: store.data().fuel,
        },
    }
}

/// Assert both engines agree, and say which axis differed if they do not.
#[track_caller]
fn assert_agree(name: &str, text: &str, input: &[u8]) {
    let module = canonical(text);
    let mine = run_cairn(&module, input);
    let reference = run_wasmi(&module, input);

    assert_eq!(
        mine.output.is_some(),
        reference.output.is_some(),
        "{name}: one engine trapped and the other did not \
         (cairn: {:?}, wasmi: {:?})",
        mine.output.as_ref().map(Vec::len),
        reference.output.as_ref().map(Vec::len),
    );
    assert_eq!(
        mine.fuel, reference.fuel,
        "{name}: fuel differs, so the two engines did not execute the same instructions",
    );
    assert_eq!(mine.output, reference.output, "{name}: output differs");
}

/// Wrap an expression in a workload that writes its `i64` value out.
///
/// `i64` rather than `i32` so that a truncation bug in either engine shows up as differing
/// bytes rather than being masked.
fn workload(body: &str) -> String {
    format!(
        r#"(module
             (import "cairn" "output" (func $output (param i32 i32)))
             (memory (export "memory") 1 8)
             (func $compute (result i64) {body})
             (func (export "cairn_run")
               (i64.store (i32.const 0) (call $compute))
               (call $output (i32.const 0) (i32.const 8))))"#
    )
}

#[test]
fn integer_arithmetic_agrees() {
    let cases = [
        ("add", "(i64.add (i64.const 20) (i64.const 22))"),
        (
            "wrapping add",
            "(i64.add (i64.const 9223372036854775807) (i64.const 1))",
        ),
        (
            "mul",
            "(i64.mul (i64.const 123456789) (i64.const 987654321))",
        ),
        ("div_s negative", "(i64.div_s (i64.const -7) (i64.const 2))"),
        ("rem_s negative", "(i64.rem_s (i64.const -7) (i64.const 2))"),
        (
            "div_u treats the operands as unsigned",
            "(i64.div_u (i64.const -7) (i64.const 2))",
        ),
        (
            "shl past the width",
            "(i64.shl (i64.const 1) (i64.const 64))",
        ),
        (
            "shr_s on a negative",
            "(i64.shr_s (i64.const -8) (i64.const 1))",
        ),
        (
            "shr_u on a negative",
            "(i64.shr_u (i64.const -8) (i64.const 1))",
        ),
        (
            "rotl",
            "(i64.rotl (i64.const 0x123456789abcdef) (i64.const 20))",
        ),
        ("clz", "(i64.clz (i64.const 1))"),
        ("ctz", "(i64.ctz (i64.const 8))"),
        ("popcnt", "(i64.popcnt (i64.const -1))"),
    ];
    for (name, body) in cases {
        assert_agree(name, &workload(body), &[]);
    }
}

#[test]
fn float_arithmetic_agrees() {
    // Every one of these produces a NaN or a signed zero somewhere, which is where the two
    // engines are most likely to differ and where the instrumentation pass is doing work.
    let cases = [
        ("sqrt of a negative", "(f64.sqrt (f64.const -1))"),
        ("zero over zero", "(f64.div (f64.const 0) (f64.const 0))"),
        (
            "infinity minus infinity",
            "(f64.sub (f64.div (f64.const 1) (f64.const 0)) (f64.div (f64.const 1) (f64.const 0)))",
        ),
        (
            "min of signed zeros",
            "(f64.min (f64.const 0) (f64.const -0))",
        ),
        (
            "max of signed zeros",
            "(f64.max (f64.const -0) (f64.const 0))",
        ),
        (
            "min with a NaN",
            "(f64.min (f64.sqrt (f64.const -1)) (f64.const 1))",
        ),
        ("nearest of a half", "(f64.nearest (f64.const 2.5))"),
        (
            "nearest of a negative half",
            "(f64.nearest (f64.const -2.5))",
        ),
        ("trunc toward zero", "(f64.trunc (f64.const -1.9))"),
        ("copysign", "(f64.copysign (f64.const 1) (f64.const -0))"),
        (
            "demote then promote loses precision",
            "(f64.promote_f32 (f32.demote_f64 (f64.const 0.1)))",
        ),
    ];
    for (name, body) in cases {
        // The result is a float, so reinterpret it to compare bit patterns rather than values.
        assert_agree(
            name,
            &workload(&format!("(i64.reinterpret_f64 {body})")),
            &[],
        );
    }
}

#[test]
fn saturating_conversions_agree() {
    let cases = [
        ("huge to i64", "(i64.trunc_sat_f64_s (f64.const 1e300))"),
        (
            "negative huge to i64",
            "(i64.trunc_sat_f64_s (f64.const -1e300))",
        ),
        (
            "NaN to i64",
            "(i64.trunc_sat_f64_s (f64.sqrt (f64.const -1)))",
        ),
        (
            "negative to unsigned",
            "(i64.trunc_sat_f64_u (f64.const -5))",
        ),
    ];
    for (name, body) in cases {
        assert_agree(name, &workload(body), &[]);
    }
}

#[test]
fn traps_agree() {
    // Both engines must stop, and must have charged the same amount before doing so -- which
    // is the part that says they trapped at the same instruction rather than merely both
    // failing.
    let cases = [
        ("unreachable", "(unreachable)"),
        (
            "divide by zero",
            "(drop (i64.div_s (i64.const 1) (i64.const 0)))",
        ),
        (
            "the one unrepresentable quotient",
            "(drop (i64.div_s (i64.const -9223372036854775808) (i64.const -1)))",
        ),
        (
            "trunc of a NaN",
            "(drop (i64.trunc_f64_s (f64.sqrt (f64.const -1))))",
        ),
        (
            "trunc out of range",
            "(drop (i64.trunc_f64_s (f64.const 1e300)))",
        ),
        (
            "load past the end of memory",
            "(drop (i64.load (i32.const 100000)))",
        ),
        (
            "store past the end of memory",
            "(i64.store (i32.const 100000) (i64.const 1))",
        ),
    ];
    for (name, body) in cases {
        let module = format!(
            r#"(module
                 (memory (export "memory") 1 8)
                 (func (export "cairn_run") {body}))"#
        );
        assert_agree(name, &module, &[]);
    }
}

#[test]
fn control_flow_agrees() {
    let cases = [
        (
            "counted loop",
            r#"(local $i i64)
               (block $done
                 (loop $again
                   (br_if $done (i64.ge_u (local.get $i) (i64.const 1000)))
                   (local.set $i (i64.add (local.get $i) (i64.const 1)))
                   (br $again)))
               (local.get $i)"#,
        ),
        (
            "if with both branches",
            "(if (result i64) (i32.const 1) (then (i64.const 10)) (else (i64.const 20)))",
        ),
        (
            "if without an else",
            r#"(local $x i64)
               (local.set $x (i64.const 5))
               (if (i32.const 0) (then (local.set $x (i64.const 99))))
               (local.get $x)"#,
        ),
        (
            "br_table dispatch",
            r#"(local $r i64)
               (block $default
                 (block $one
                   (block $zero
                     (br_table $zero $one $default (i32.const 1)))
                   (local.set $r (i64.const 100))
                   (br $default))
                 (local.set $r (i64.const 200))
                 (br $default))
               (local.get $r)"#,
        ),
        (
            "nested blocks with results",
            r#"(block $outer (result i64)
                 (block $inner (result i64)
                   (br $outer (i64.const 7)))
                 (drop)
                 (i64.const 9))"#,
        ),
        (
            "select",
            "(select (i64.const 1) (i64.const 2) (i32.const 0))",
        ),
    ];
    for (name, body) in cases {
        assert_agree(name, &workload(body), &[]);
    }
}

#[test]
fn calls_and_recursion_agree() {
    let module = r#"(module
         (import "cairn" "output" (func $output (param i32 i32)))
         (memory (export "memory") 1 8)
         (func $fib (param $n i64) (result i64)
           (if (result i64) (i64.lt_u (local.get $n) (i64.const 2))
             (then (local.get $n))
             (else (i64.add
                     (call $fib (i64.sub (local.get $n) (i64.const 1)))
                     (call $fib (i64.sub (local.get $n) (i64.const 2)))))))
         (func (export "cairn_run")
           (i64.store (i32.const 0) (call $fib (i64.const 18)))
           (call $output (i32.const 0) (i32.const 8))))"#;
    assert_agree("recursive fibonacci", module, &[]);
}

#[test]
fn indirect_calls_agree() {
    let module = r#"(module
         (import "cairn" "output" (func $output (param i32 i32)))
         (memory (export "memory") 1 8)
         (type $sig (func (param i64) (result i64)))
         (table 3 3 funcref)
         (func $double (type $sig) (i64.mul (local.get 0) (i64.const 2)))
         (func $square (type $sig) (i64.mul (local.get 0) (local.get 0)))
         (func $negate (type $sig) (i64.sub (i64.const 0) (local.get 0)))
         (elem (i32.const 0) $double $square $negate)
         (func (export "cairn_run") (local $acc i64) (local $i i32)
           (local.set $acc (i64.const 3))
           (block $done
             (loop $again
               (br_if $done (i32.ge_u (local.get $i) (i32.const 3)))
               (local.set $acc (call_indirect (type $sig) (local.get $acc) (local.get $i)))
               (local.set $i (i32.add (local.get $i) (i32.const 1)))
               (br $again)))
           (i64.store (i32.const 0) (local.get $acc))
           (call $output (i32.const 0) (i32.const 8))))"#;
    assert_agree("indirect dispatch through a table", module, &[]);
}

#[test]
fn memory_operations_agree() {
    let cases = [
        (
            "narrow store then signed load",
            r#"(i32.store8 (i32.const 16) (i32.const 0xff))
               (i64.extend_i32_s (i32.load8_s (i32.const 16)))"#,
        ),
        (
            "narrow store then unsigned load",
            r#"(i32.store8 (i32.const 16) (i32.const 0xff))
               (i64.extend_i32_u (i32.load8_u (i32.const 16)))"#,
        ),
        (
            "static offset",
            r#"(i64.store offset=8 (i32.const 100) (i64.const 7))
               (i64.load (i32.const 108))"#,
        ),
        (
            "fill",
            r#"(memory.fill (i32.const 32) (i32.const 0xab) (i32.const 8))
               (i64.load (i32.const 32))"#,
        ),
        (
            "overlapping copy",
            r#"(i64.store (i32.const 40) (i64.const 0x1122334455667788))
               (memory.copy (i32.const 44) (i32.const 40) (i32.const 8))
               (i64.load (i32.const 44))"#,
        ),
        (
            "grow reports the previous size",
            "(i64.extend_i32_s (memory.grow (i32.const 2)))",
        ),
        (
            "grow past the declared maximum fails",
            "(i64.extend_i32_s (memory.grow (i32.const 99)))",
        ),
        (
            "size after growing",
            r#"(drop (memory.grow (i32.const 3)))
               (i64.extend_i32_u (memory.size))"#,
        ),
    ];
    for (name, body) in cases {
        assert_agree(name, &workload(body), &[]);
    }
}

#[test]
fn globals_and_data_segments_agree() {
    let module = r#"(module
         (import "cairn" "output" (func $output (param i32 i32)))
         (memory (export "memory") 1 8)
         (global $counter (mut i64) (i64.const 100))
         (data (i32.const 0) "\01\02\03\04\05\06\07\08")
         (func (export "cairn_run")
           (global.set $counter (i64.add (global.get $counter) (i64.load (i32.const 0))))
           (i64.store (i32.const 16) (global.get $counter))
           (call $output (i32.const 16) (i32.const 8))))"#;
    assert_agree("globals and data", module, &[]);
}

#[test]
fn input_handling_agrees() {
    let module = r#"(module
         (import "cairn" "input"  (func $input  (param i32 i32) (result i32)))
         (import "cairn" "output" (func $output (param i32 i32)))
         (memory (export "memory") 1 8)
         (func (export "cairn_run") (local $len i32)
           (local.set $len (call $input (i32.const 0) (i32.const 0)))
           (drop (call $input (i32.const 64) (local.get $len)))
           (call $output (i32.const 64) (local.get $len))))"#;

    for input in [
        b"".as_slice(),
        b"a".as_slice(),
        b"a longer volunteer message".as_slice(),
    ] {
        assert_agree("input round trip", module, input);
    }
}

#[test]
fn a_deliberate_divergence_is_caught() {
    // A harness that cannot fail proves nothing. Two different workloads must disagree, or
    // every assertion above is vacuous.
    let a = canonical(&workload("(i64.const 1)"));
    let b = canonical(&workload("(i64.const 2)"));

    assert_ne!(run_cairn(&a, &[]), run_cairn(&b, &[]));
    assert_ne!(run_wasmi(&a, &[]), run_wasmi(&b, &[]));

    // And the reference engine must genuinely be executing, not silently returning nothing.
    let outcome = run_wasmi(&a, &[]);
    assert_eq!(outcome.output, Some(1i64.to_le_bytes().to_vec()));
    assert!(outcome.fuel > 0, "the reference engine charged no fuel");
}
