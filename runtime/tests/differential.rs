//! Differential testing: Cairn's interpreter against three independent WebAssembly engines,
//! one of which is the engine volunteers actually run.
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
//! answers written by the same person who wrote it. It is tested against two mature engines
//! developed independently, on the same bytes.
//!
//! # Why two references, and why of different kinds
//!
//! [`wasmi`](https://docs.rs/wasmi) interprets. [`wasmtime`](https://docs.rs/wasmtime) compiles
//! through Cranelift. That difference is the point: a compiler can go wrong in ways an
//! interpreter cannot — folding a float expression at compile time, contracting a multiply and
//! an add, reassociating arithmetic — and those are exactly the transformations that would
//! break a scheme resting on bit-exact agreement. Agreement between an interpreter and a JIT
//! is far stronger evidence than agreement between two interpreters.
//!
//! # The third reference is the point of the whole project
//!
//! wasmi and wasmtime are both Rust, both linked into this binary, and **neither is the engine
//! this project is for**. A volunteer opens a tab and the unit runs on V8 or SpiderMonkey.
//! [`the_browsers_own_engine_agrees_with_cairn`] sends the generated corpus through
//! `browser/differential.js`, which imports the volunteer's own `host.js` — the same three host
//! functions `worker.js` uses — and compares what came back. It costs a process boundary and it
//! is worth it: until it existed, the engine at the centre of the design was the only one in the
//! system that nothing checked.
//!
//! **It has teeth, and the proof is unusually direct.** Delete the `f64.copysign` escape site
//! from `canon::escape_site` and V8 immediately disagrees with Cairn's interpreter: `+1.5`
//! against `-1.5`, on `float case 2` under the honest configuration. So
//! [ADR-0006](../../docs/adr/0006-canonicalize-nans-at-escapes-on-the-honest-path.md)'s
//! reasoning about NaN signs is not defensive programming against a hypothetical engine — the
//! engine volunteers actually use **does** choose the other sign, and canonicalizing it is what
//! stops an honest browser volunteer from losing a dispute it could not win.
//!
//! # Where the cases come from
//!
//! Hand-written cases cover the divergences someone thought of, which is the wrong coverage
//! model for a component whose failure mode is convicting an honest volunteer. So there are
//! also two seeded generators, and both have already earned their place:
//!
//! - [`random_float_expressions_agree_across_engines`] builds float expression trees, aimed at
//!   `canon::escape_site`. It caught a deliberately removed `copysign` escape that the
//!   hand-written cases could not reach.
//! - [`generated_modules_agree_across_engines`] builds whole modules with `wasm-smith`, aimed
//!   at everything else — control flow, memory, calls, and combinations. **On its first run it
//!   found a real bug**: `br 0` at function scope names WebAssembly's implicit function label
//!   and returns, and the interpreter had no such label, so it trapped with an internal
//!   `StackUnderflow` on a module both references ran to completion.
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

use cairn_runtime::canon::{self, Canonicalization, Config, Metering};
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
    /// Cairn stopped on a ceiling of its own that no other engine shares.
    ///
    /// Never set by the reference engines, and **not a divergence**. Two of Cairn's limits are
    /// deliberately unlike an ordinary engine's, for determinism: the call-depth limit is an
    /// explicit number rather than whatever the host stack happens to allow, and the fuel
    /// ceiling exists at all. A module that reaches either will stop in Cairn and keep going
    /// elsewhere, correctly. The hand-written corpus stays well inside both; generated modules
    /// do not, so they are skipped rather than compared.
    hit_a_cairn_limit: bool,
}

/// Assemble, validate and instrument, exactly as a coordinator would.
fn canonical(text: &str) -> Vec<u8> {
    canonical_with(text, Config::default())
}

/// The same, under a chosen instrumentation configuration.
fn canonical_with(text: &str, config: Config) -> Vec<u8> {
    canonical_bytes(
        &wat::parse_str(text).expect("module should assemble"),
        config,
    )
}

/// The same again, for a module that arrived as bytes rather than as text.
///
/// Everything else in this file writes its cases in WebAssembly's text format. The math library
/// does not: it is compiled from Rust by a real toolchain, which is the point of it.
fn canonical_bytes(source: &[u8], config: Config) -> Vec<u8> {
    validate::validate_submitted(source, validate::Limits::default())
        .expect("module should be a valid Cairn workload");
    canon::instrument(source, config).expect("instrumentation should succeed")
}

/// Both settings a volunteer could plausibly run, checked against the fully instrumented module.
///
/// Metering and snapshots are absent from both because the fast path cannot use them — see
/// [ADR-0005](../../docs/adr/0005-the-fast-path-cannot-snapshot.md). Either variant therefore
/// has to produce the same answer as the fully instrumented one, or a dispute would be
/// arbitrating a different execution from the one whose result was submitted.
///
/// `AtEscapes` is the interesting one and the one shipped
/// ([ADR-0006](../../docs/adr/0006-canonicalize-nans-at-escapes-on-the-honest-path.md)): it
/// leaves engine-specific NaN payloads in flight and only fixes them where they could become
/// observable. If that reasoning is wrong anywhere, it shows up here as a result that differs
/// from the module which canonicalizes everything.
const HONEST_CONFIGS: [Config; 2] = [
    Config::honest_path(),
    Config {
        meter: Metering::Off,
        canonicalize: Canonicalization::Everywhere,
    },
];

/// Run under Cairn's interpreter.
fn run_cairn(module: &[u8], input: &[u8]) -> Outcome {
    use cairn_runtime::engine::numeric::Trap;

    let image = image::decode(module).expect("instrumented module should decode");
    let mut machine = match Machine::new(&image, input.to_vec(), Limits::default()) {
        Ok(machine) => machine,
        Err(_) => {
            return Outcome {
                output: None,
                fuel: 0,
                hit_a_cairn_limit: true,
            }
        }
    };
    match machine.run() {
        Ok(trace) => Outcome {
            output: Some(trace.output),
            fuel: trace.fuel.get(),
            hit_a_cairn_limit: false,
        },
        Err(trap) => Outcome {
            output: None,
            fuel: machine.fuel().get(),
            hit_a_cairn_limit: matches!(trap, Trap::OutOfFuel | Trap::CallStackExhausted),
        },
    }
}

impl Outcome {
    /// What a reference engine reports. `hit_a_cairn_limit` is Cairn's alone by construction.
    fn reference(output: Option<Vec<u8>>, fuel: u64) -> Self {
        Self {
            output,
            fuel,
            hit_a_cairn_limit: false,
        }
    }
}

/// Does `[ptr, ptr + len)` lie inside a memory of `size` bytes?
///
/// Both arguments arrive from the guest and are attacker-controlled in exactly the sense that
/// matters here: `wasm-smith` will produce every value an `i32` can hold.
fn fits(size: usize, ptr: i32, len: i32) -> bool {
    let ptr = ptr as u32 as usize;
    let len = len as u32 as usize;
    ptr.checked_add(len).is_some_and(|end| end <= size)
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
                let memory = caller
                    .get_export("memory")
                    .and_then(Extern::into_memory)
                    .expect("workload exports its memory");
                // Bounds-check before allocating, not after. A generated workload will happily
                // ask for 4 GiB, and `vec![0u8; len]` would take the whole harness down with
                // it — a hazard that only appears once the corpus stops being hand-written.
                if !fits(memory.data_size(&caller), ptr, len) {
                    return;
                }
                let mut buffer = vec![0u8; len as u32 as usize];
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
        return Outcome::reference(None, store.data().fuel);
    };

    let entry = instance
        .get_typed_func::<(), ()>(&store, validate::ENTRY_POINT)
        .expect("workload exports its entry point");

    let result = entry.call(&mut store, ());

    // Under `Metering::Global` there are no `charge` calls to accumulate; the count is in a
    // global the module exports, and reading it back out is the entire claim that encoding
    // makes. Reading it *after* a trap matters too — a partial count is still a count.
    let fuel = match instance.get_global(&store, validate::FUEL_EXPORT) {
        Some(global) => match global.get(&store) {
            wasmi::Val::I64(n) => n as u64,
            other => panic!("the counter must be an i64, got {other:?}"),
        },
        None => store.data().fuel,
    };

    match result {
        Ok(()) => Outcome::reference(Some(store.data().output.clone()), fuel),
        Err(_) => Outcome::reference(None, fuel),
    }
}

/// Run under wasmtime — a compiler, not an interpreter.
///
/// The second reference engine, and deliberately a different *kind* of engine. `wasmi`
/// interprets; wasmtime lowers through Cranelift to machine code. Agreement between an
/// interpreter and a JIT is much stronger evidence than agreement between two interpreters,
/// because the ways a compiler can go wrong — constant folding a float expression, choosing a
/// fused multiply-add, reassociating arithmetic — are not available to an interpreter at all.
///
/// It is also the closest thing in this repository to the fast path, which is a browser JIT
/// and does not exist yet ([ADR-0005](../../docs/adr/0005-the-fast-path-cannot-snapshot.md)).
fn run_wasmtime(module: &[u8], input: &[u8]) -> Outcome {
    use wasmtime::{Caller, Engine, Extern, Linker, Module, Store};

    let engine = Engine::default();
    let Ok(module) = Module::new(&engine, module) else {
        return Outcome::reference(None, 0);
    };
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
                let memory = caller
                    .get_export("memory")
                    .and_then(Extern::into_memory)
                    .expect("workload exports its memory");
                // Bounds-check before allocating; see the note on the wasmi side.
                if !fits(memory.data_size(&caller), ptr, len) {
                    return;
                }
                let mut buffer = vec![0u8; len as u32 as usize];
                if memory
                    .read(&caller, ptr as u32 as usize, &mut buffer)
                    .is_ok()
                {
                    caller.data_mut().output = buffer;
                }
            },
        )
        .expect("output should link");

    let Ok(instance) = linker.instantiate(&mut store, &module) else {
        return Outcome::reference(None, store.data().fuel);
    };

    let entry = instance
        .get_typed_func::<(), ()>(&mut store, validate::ENTRY_POINT)
        .expect("workload exports its entry point");

    let result = entry.call(&mut store, ());

    // As on the wasmi side: the global-metered module reports through an export rather than a
    // host call. This is the one place in the repository where a *compiler* is asked for the
    // instruction count, which is what that encoding exists to make possible.
    let fuel = match instance.get_global(&mut store, validate::FUEL_EXPORT) {
        Some(global) => match global.get(&mut store) {
            wasmtime::Val::I64(n) => n as u64,
            other => panic!("the counter must be an i64, got {other:?}"),
        },
        None => store.data().fuel,
    };

    match result {
        Ok(()) => Outcome::reference(Some(store.data().output.clone()), fuel),
        Err(_) => Outcome::reference(None, fuel),
    }
}

/// Assert all three engines agree, and say which axis differed if they do not.
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

    // And against the JIT. Kept as a separate block so a failure says which reference
    // disagreed — "wasmi agrees but wasmtime does not" points at compilation, and is a very
    // different investigation from both references disagreeing.
    let compiled = run_wasmtime(&module, input);
    assert_eq!(
        mine.output.is_some(),
        compiled.output.is_some(),
        "{name}: cairn and wasmtime disagree about whether execution trapped",
    );
    assert_eq!(
        mine.fuel, compiled.fuel,
        "{name}: fuel differs against wasmtime, so the JIT took a different path",
    );
    assert_eq!(
        mine.output, compiled.output,
        "{name}: output differs against wasmtime",
    );

    assert_metering_encodings_agree(name, text, input, &mine);
    assert_instrumentation_is_transparent(name, text, input, &mine);
}

/// Assert that how a module counts does not change what it counts, or what it computes.
///
/// [`Metering::Global`] replaces the per-block host call with an addition into a counter the
/// module exports. The reason it exists is that a compiler charges an enormous premium for the
/// host call and none for the addition — but a cheaper encoding is worthless if it produces a
/// different number, so this checks the number.
///
/// The two reference engines here are doing something they do nowhere else in this file: they
/// are reporting an instruction count **they were not told**. Under the host-call encoding the
/// harness accumulates the total itself, one call at a time, which proves little beyond that
/// the calls happened. Here the module keeps its own count and the engine hands the global
/// back at the end, so agreement means the engine executed the same basic blocks in the same
/// order — and wasmtime reaching the same total as Cairn's interpreter is the closest this
/// repository comes to evidence that a volunteer's own engine could report its work honestly.
#[track_caller]
fn assert_metering_encodings_agree(name: &str, text: &str, input: &[u8], by_host_call: &Outcome) {
    let module = canonical_with(
        text,
        Config {
            meter: Metering::Global,
            ..Config::default()
        },
    );

    for (engine, outcome) in [
        ("cairn", run_cairn(&module, input)),
        ("wasmi", run_wasmi(&module, input)),
        ("wasmtime", run_wasmtime(&module, input)),
    ] {
        assert_eq!(
            outcome.output, by_host_call.output,
            "{name}: {engine} computed something else under global metering",
        );
        assert_eq!(
            outcome.fuel, by_host_call.fuel,
            "{name}: {engine} counted {} instructions through the exported global where the \
             host call counted {}",
            outcome.fuel, by_host_call.fuel,
        );
    }
}

/// Assert that adding metering does not change what a workload computes.
///
/// Cairn's honest path runs the determinism-only module and returns just a result; a trace is
/// produced later, by re-executing the *metered* module, only if someone disputes that result
/// ([ADR-0005](../../docs/adr/0005-the-fast-path-cannot-snapshot.md)). The whole scheme rests
/// on those two modules being the same program: if metering could change an answer or turn a
/// completed run into a trap, arbitration would settle a dispute about an execution that never
/// happened, and it would do so against an honest worker.
///
/// Fuel is deliberately not compared — the determinism-only module has no `charge` calls to
/// count, which is the entire point of running it.
#[track_caller]
fn assert_instrumentation_is_transparent(name: &str, text: &str, input: &[u8], metered: &Outcome) {
    for config in HONEST_CONFIGS {
        let module = canonical_with(text, config);
        let plain = run_cairn(&module, input);

        assert_eq!(
            plain.output.is_some(),
            metered.output.is_some(),
            "{name}: {config:?} changed whether execution trapped",
        );
        assert_eq!(
            plain.output, metered.output,
            "{name}: {config:?} changed the result",
        );

        // And under both independent engines, since the fast path is not ours. This is the
        // check that would catch a wrong entry in `escape_site`: engines choosing different
        // NaN payloads only disagree observably if a payload escaped. The JIT matters most
        // here — a compiler has far more freedom in how it evaluates float expressions, so it
        // is the likeliest of the three to produce a payload the others do not.
        for (engine, outcome) in [
            ("wasmi", run_wasmi(&module, input)),
            ("wasmtime", run_wasmtime(&module, input)),
        ] {
            assert_eq!(
                outcome.output, metered.output,
                "{name}: {engine} disagrees under {config:?}",
            );
        }
    }
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

/// Try to get an engine-specific NaN payload to change an answer.
///
/// Every case here computes a NaN, drags it through as much of the instruction set as it can,
/// and then makes it observable. `assert_agree` runs each under the fully canonicalizing
/// module *and* under `Canonicalization::AtEscapes`, on both engines, and requires all four to
/// produce the same bytes.
///
/// This is the test that would catch a missing entry in `canon::escape_site`, and it has been
/// checked against that: deleting `I64ReinterpretF64` from the escape set makes this test fail
/// on the payload `0x…001` leaking through, and makes `float_arithmetic_agrees` fail too.
///
/// That second failure is worth knowing about. It comes from `sqrt` of a negative number,
/// which on both of these engines returns a NaN with the **sign bit set** — `0xfff8…` rather
/// than `0x7ff8…`. So a computed NaN's sign really does vary in practice, which is what makes
/// `copysign` a genuine escape site rather than a theoretical one: `copysign(1.0, sqrt(-1.0))`
/// hands that sign to an ordinary number.
#[test]
fn nan_payloads_cannot_escape() {
    // Kept out of `workload` because these need globals, and because the value being made
    // observable is the point rather than an i64 expression's result.
    fn module(globals: &str, body: &str) -> String {
        format!(
            r#"(module
                 (import "cairn" "output" (func $output (param i32 i32)))
                 (memory (export "memory") 1 8)
                 {globals}
                 (func (export "cairn_run")
                   {body}
                   (call $output (i32.const 0) (i32.const 8))))"#
        )
    }

    // A NaN with a payload that is *not* the canonical one, built by the program itself.
    //
    // `0.0/0.0` would be the obvious choice and is useless here: both engines return the
    // canonical pattern for it, so canonicalizing and not canonicalizing produce identical
    // bytes and every assertion below would hold no matter what `escape_site` returned.
    // Starting from a distinct payload and pushing it through arithmetic — which propagates it
    // on both of these engines — is what gives these cases the ability to fail.
    const RAW: &str = "(f64.reinterpret_i64 (i64.const 0x7ff8000000000001))";
    const NAN: &str = "(f64.add (f64.reinterpret_i64 (i64.const 0x7ff8000000000001)) \
                       (f64.const 1))";

    let cases = [
        (
            "reinterpret exposes the payload directly",
            module(
                "",
                &format!("(i64.store (i32.const 0) (i64.reinterpret_f64 {NAN}))"),
            ),
        ),
        (
            "arithmetic keeps it a NaN, the store makes it visible",
            module(
                "",
                &format!(
                    r#"(local $n f64)
                       (local.set $n {NAN})
                       (local.set $n (f64.add (local.get $n) (f64.const 1)))
                       (local.set $n (f64.mul (local.get $n) (f64.const 2)))
                       (local.set $n (f64.min (local.get $n) (f64.const 3)))
                       (local.set $n (f64.max (local.get $n) (f64.const 4)))
                       (local.set $n (f64.sqrt (local.get $n)))
                       (local.set $n (f64.abs (f64.neg (local.get $n))))
                       (f64.store (i32.const 0) (local.get $n))"#
                ),
            ),
        ),
        (
            "a NaN parked in a global",
            module(
                "(global $g (mut f64) (f64.const 0))",
                &format!(
                    r#"(global.set $g {NAN})
                       (i64.store (i32.const 0) (i64.reinterpret_f64 (global.get $g)))"#
                ),
            ),
        ),
        (
            "copysign reads a computed NaN's sign",
            module(
                "",
                &format!("(f64.store (i32.const 0) (f64.copysign (f64.const 1) {NAN}))"),
            ),
        ),
        (
            "comparisons against a NaN steer control flow",
            module(
                "",
                &format!(
                    r#"(local $n f64)
                       (local.set $n {NAN})
                       (if (f64.lt (local.get $n) (f64.const 1))
                         (then (i64.store (i32.const 0) (i64.const 111)))
                         (else (if (f64.ne (local.get $n) (local.get $n))
                                 (then (i64.store (i32.const 0) (i64.const 222)))
                                 (else (i64.store (i32.const 0) (i64.const 333))))))"#
                ),
            ),
        ),
        (
            "saturating truncation of a NaN",
            module(
                "",
                &format!(
                    "(i64.store (i32.const 0) (i64.extend_i32_s (i32.trunc_sat_f64_s {NAN})))"
                ),
            ),
        ),
        (
            // The program built this NaN itself and never ran a NaN-producing operation, so
            // its payload is its own data, identical on every engine. Both modes still have to
            // agree about what comes out — which is why `Everywhere` canonicalizes at escapes
            // as well, rather than only after arithmetic.
            "a payload the program chose, read straight back",
            module(
                "",
                &format!("(i64.store (i32.const 0) (i64.reinterpret_f64 {RAW}))"),
            ),
        ),
        (
            "f32 by the same routes",
            module(
                "",
                r#"(local $n f32)
                   (local.set $n (f32.reinterpret_i32 (i32.const 0x7fc00001)))
                   (local.set $n (f32.add (local.get $n) (f32.const 1)))
                   (local.set $n (f32.max (local.get $n) (f32.const 2)))
                   (i64.store (i32.const 0)
                     (i64.extend_i32_u (i32.reinterpret_f32 (local.get $n))))"#,
            ),
        ),
        (
            "demote and promote across widths",
            module(
                "",
                &format!(
                    r#"(i64.store (i32.const 0)
                         (i64.reinterpret_f64 (f64.promote_f32 (f32.demote_f64 {NAN}))))"#
                ),
            ),
        ),
    ];

    for (name, text) in &cases {
        assert_agree(name, text, &[]);
    }
}

/// A tiny deterministic PRNG, so a failing case is reproducible from its seed alone.
///
/// xorshift64*, chosen because it is four lines and has no dependency. Statistical quality is
/// irrelevant here — this picks between a dozen enum variants, it does not model anything.
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

/// Build a random floating-point expression.
///
/// Weighted towards the values that make NaN handling interesting — non-canonical payloads,
/// both signed zeroes, both infinities, and the operand pairs that mint a NaN out of finite
/// inputs (`inf - inf`, `0 * inf`, `0 / 0`, `sqrt` of a negative).
fn random_f64(rng: &mut Rng, depth: u32) -> String {
    const LEAVES: [&str; 10] = [
        "(f64.const 0)",
        "(f64.const -0.0)",
        "(f64.const 1.5)",
        "(f64.const -3.25)",
        "(f64.const inf)",
        "(f64.const -inf)",
        "(f64.const 1e308)",
        // A NaN with a payload that is not the canonical one, and one with the sign bit set.
        "(f64.reinterpret_i64 (i64.const 0x7ff8000000000001))",
        "(f64.reinterpret_i64 (i64.const 0xfff8000000000003))",
        "(local.get $x)",
    ];
    const UNARY: [&str; 7] = [
        "f64.sqrt",
        "f64.abs",
        "f64.neg",
        "f64.ceil",
        "f64.floor",
        "f64.trunc",
        "f64.nearest",
    ];
    const BINARY: [&str; 7] = [
        "f64.add",
        "f64.sub",
        "f64.mul",
        "f64.div",
        "f64.min",
        "f64.max",
        "f64.copysign",
    ];

    if depth == 0 {
        return LEAVES[rng.pick(LEAVES.len())].to_owned();
    }
    match rng.pick(10) {
        0..=2 => {
            let op = UNARY[rng.pick(UNARY.len())];
            format!("({op} {})", random_f64(rng, depth - 1))
        }
        // Round-tripping through f32 exercises demote and promote, both of which can choose a
        // NaN payload, and narrows the value enough to reach the infinities.
        3 => format!(
            "(f64.promote_f32 (f32.demote_f64 {}))",
            random_f64(rng, depth - 1)
        ),
        _ => {
            let op = BINARY[rng.pick(BINARY.len())];
            format!(
                "({op} {} {})",
                random_f64(rng, depth - 1),
                random_f64(rng, depth - 1)
            )
        }
    }
}

/// Randomised float soup, checked across three engines and both instrumentation settings.
///
/// # Why this exists and why it is shaped like this
///
/// The hand-written corpus tests the divergences someone thought of. `canon::escape_site` is a
/// hand-written table whose failure mode is **convicting an honest volunteer**, so "the cases
/// someone thought of" is exactly the wrong coverage model for it. This generates expression
/// trees over every float operation Cairn admits, seeded from the values most likely to mint
/// or carry an unusual NaN, and requires Cairn's interpreter, `wasmi` and `wasmtime` to agree
/// under full instrumentation *and* under the honest path.
///
/// Seeds are fixed rather than drawn from the clock. A determinism gate that tests something
/// different on every run cannot be bisected when it fails, and would turn a real divergence
/// into an unreproducible flake.
///
/// It has been checked against the mistake it exists to catch, and it found the one the
/// hand-written cases could not: deleting `F64Copysign` from the escape set makes this fail on
/// case 2, with `-1.5` against `+1.5` — no NaN anywhere in the answer, just a sign flipped by
/// reading the sign bit of a NaN that the two engines produced differently.
///
/// It is not `wasm-smith`. That would cover far more of the instruction set, and it is the
/// listed follow-up; what it would *not* do without substantial plumbing is concentrate on
/// float expression shapes, which is where the newest and least-proven reasoning in this
/// repository lives.
#[test]
fn random_float_expressions_agree_across_engines() {
    const CASES: u32 = 300;
    let mut rng = Rng(0x5eed_1234_abcd_ef01);

    for case in 0..CASES {
        let seed = rng.0;
        let depth = 2 + (case % 3);
        let expression = random_f64(&mut rng, depth);

        // Three ways out: as raw bytes, as an integer holding the payload, and through
        // `copysign`, which reads the sign of a computed NaN rather than its payload.
        let escape = match case % 3 {
            0 => format!("(f64.store (i32.const 0) {expression})"),
            1 => format!("(i64.store (i32.const 0) (i64.reinterpret_f64 {expression}))"),
            _ => format!("(f64.store (i32.const 0) (f64.copysign (f64.const 1.5) {expression}))"),
        };

        let text = format!(
            r#"(module
                 (import "cairn" "output" (func $output (param i32 i32)))
                 (memory (export "memory") 1 8)
                 (global $g (mut f64) (f64.const 0))
                 (func (export "cairn_run") (local $x f64)
                   (local.set $x (f64.reinterpret_i64 (i64.const 0x7ff8000000000005)))
                   (global.set $g {expression})
                   {escape}
                   (call $output (i32.const 0) (i32.const 8))))"#
        );

        // The seed is in the failure message because that is the only thing that makes a
        // generated failure actionable: it regenerates this exact module.
        assert_agree(&format!("random case {case} (seed {seed:#x})"), &text, &[]);
    }
}

/// Generate a whole module, shaped as a Cairn workload and using only admitted features.
///
/// # Why this needs so much configuration
///
/// `wasm-smith` produces arbitrary *valid WebAssembly*, and Cairn accepts a strict subset of
/// that. Two kinds of constraint have to be imposed and they are imposed differently.
///
/// **Features** are flags. Everything Cairn refuses is switched off here rather than filtered
/// out afterwards, because a generator that spends its randomness on modules the validator
/// will reject is a benchmark of the validator, not of the engines.
///
/// **Shape** is harder, and is what `available_imports` and `exports` are for. A Cairn workload
/// must export `cairn_run` and `memory`, import only from `cairn`, and declare a memory
/// maximum. `wasm-smith` will not produce that on its own; given those two template modules it
/// produces exactly it, with arbitrary bodies behind.
fn generated_module(seed: u64) -> Option<Vec<u8>> {
    let mut config = wasm_smith::Config {
        // Shape.
        available_imports: Some(
            wat::parse_str(
                r#"(module
                     (import "cairn" "input"  (func (param i32 i32) (result i32)))
                     (import "cairn" "output" (func (param i32 i32))))"#,
            )
            .ok()?,
        ),
        exports: Some(
            wat::parse_str(
                r#"(module
                     (func (export "cairn_run"))
                     (memory (export "memory") 1 4))"#,
            )
            .ok()?,
        ),
        ..wasm_smith::Config::default()
    };

    // Admitted, per `validate::admitted_features`.
    config.bulk_memory_enabled = true;
    config.multi_value_enabled = true;
    config.saturating_float_to_int_enabled = true;
    config.sign_extension_ops_enabled = true;

    // Refused. Each for a stated reason in ADR-0003 or `validate.rs`: nondeterministic
    // (threads, relaxed SIMD, under-specified SIMD corners) or uncommittable (reference types
    // and GC have no host-independent hash; custom page sizes break the page tree's assumption).
    config.reference_types_enabled = false;
    config.gc_enabled = false;
    config.simd_enabled = false;
    config.relaxed_simd_enabled = false;
    config.threads_enabled = false;
    config.shared_everything_threads_enabled = false;
    config.exceptions_enabled = false;
    config.memory64_enabled = false;
    config.tail_call_enabled = false;
    config.custom_page_sizes_enabled = false;
    config.extended_const_enabled = false;
    config.wide_arithmetic_enabled = false;

    // A start section would run code outside `cairn_run`, where nothing is metered.
    config.allow_start_export = false;
    config.memory_max_size_required = true;

    // Zero, not one. Whatever the required exports need is generated *on top of* these limits
    // — the documentation says so — so `max_memories: 1` produced two memories every time and
    // the validator refused every module for `multiple memories`. Asking for none leaves the
    // export template's memory as the only one.
    config.min_memories = 0;
    config.max_memories = 0;

    // Enough randomness to build a module of a useful size; `Unstructured` simply runs out and
    // finishes the module early when it is exhausted, which is a legitimate outcome.
    let mut entropy = Vec::with_capacity(4096);
    let mut rng = Rng(seed | 1);
    while entropy.len() < 4096 {
        entropy.extend_from_slice(&rng.next().to_le_bytes());
    }

    let mut unstructured = arbitrary::Unstructured::new(&entropy);
    let mut module = wasm_smith::Module::new(config, &mut unstructured).ok()?;

    // Guarantees the module halts, whichever engine runs it, by injecting a fuel counter into
    // the module itself. Exactly the philosophy Cairn uses for determinism — put the property
    // in the bytes rather than trusting each engine to enforce it — and it is why this test
    // cannot hang. A budget the engines disagreed about would be useless.
    module.ensure_termination(100_000).ok()?;

    Some(module.to_bytes())
}

/// Whole randomly generated modules, checked across three engines.
///
/// # What this covers that the other generator does not
///
/// [`random_float_expressions_agree_across_engines`] targets `canon::escape_site` by building
/// float expression trees. This targets everything else: control flow, memory operations,
/// calls, tables, globals, and the combinations of them nobody would think to write down.
///
/// # What it does not cover, and why
///
/// Only the **fully instrumented** module is compared here. The honest-path comparison lives
/// in the float generator and in the hand-written corpus, where every case is known to
/// terminate. A generated module halts only because `ensure_termination` injected a counter
/// into it, and that counter is not present in — and has nothing to do with — Cairn's own
/// metering, so pairing the two configurations on generated code would be comparing two
/// different termination stories rather than two instrumentation levels.
///
/// Modules the validator refuses are skipped and counted. The count is printed rather than
/// asserted: what it should be depends on `wasm-smith`'s version and on the config above, and
/// a test that pinned it would fail on an upgrade for no reason. What *is* asserted is that
/// the skip rate leaves a useful number of modules actually executed — a silently empty corpus
/// would pass every assertion in this file.
#[test]
fn generated_modules_agree_across_engines() {
    const CASES: u64 = 200;

    let mut executed = 0u32;
    let mut refused = 0u32;
    let mut ungeneratable = 0u32;
    let mut exhausted = 0u32;
    let mut reasons: Vec<String> = Vec::new();

    for case in 0..CASES {
        let seed = 0x9e37_79b9_7f4a_7c15u64.wrapping_mul(case + 1);
        let Some(source) = generated_module(seed) else {
            ungeneratable += 1;
            continue;
        };

        // A generated module can still miss Cairn's admission rules — module size, memory
        // ceiling, an entry-point signature the config cannot constrain. Refusing it is the
        // validator working, not the generator failing.
        if let Err(rejection) = validate::validate_submitted(&source, validate::Limits::default()) {
            refused += 1;
            // Kept as distinct reasons rather than a bare count. A skip rate that is high for
            // an understood reason is fine; one that is high for an unknown reason means the
            // generator is mostly exercising the validator, and the difference should not need
            // a debugging session to establish.
            // Without dropping the offset every refusal looks distinct and the list is
            // useless — which is how the "multiple memories" cause stayed hidden the first
            // time this ran.
            let full = format!("{rejection}");
            let reason = full.split(" (at offset").next().unwrap_or(&full).to_owned();
            if !reasons.contains(&reason) {
                reasons.push(reason);
            }
            continue;
        }

        let Ok(module) = canon::instrument(&source, Config::default()) else {
            refused += 1;
            continue;
        };

        let name = format!("generated case {case} (seed {seed:#x})");
        let mine = run_cairn(&module, &[]);

        // Cairn's own ceilings are not divergences — see `Outcome::hit_a_cairn_limit`.
        if mine.hit_a_cairn_limit {
            exhausted += 1;
            continue;
        }

        for (engine, theirs) in [
            ("wasmi", run_wasmi(&module, &[])),
            ("wasmtime", run_wasmtime(&module, &[])),
        ] {
            assert_eq!(
                mine.output.is_some(),
                theirs.output.is_some(),
                "{name}: cairn and {engine} disagree about whether execution trapped",
            );
            assert_eq!(
                mine.fuel, theirs.fuel,
                "{name}: fuel differs against {engine}, so they took different paths",
            );
            assert_eq!(
                mine.output, theirs.output,
                "{name}: output differs against {engine}"
            );
        }
        executed += 1;
    }

    println!(
        "generated modules: {executed} executed, {refused} refused by the validator, \
         {exhausted} hit a Cairn-only limit, {ungeneratable} not generatable"
    );
    if !reasons.is_empty() {
        println!("  refusals: {}", reasons.join(" · "));
    }
    assert!(
        executed >= CASES as u32 / 4,
        "only {executed} of {CASES} generated modules reached the engines — the corpus has \
         gone empty and every assertion above is vacuous"
    );
}

#[test]
#[ignore = "diagnostic"]
fn probe_generated_case() {
    let case: u64 = std::env::var("CAIRN_CASE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4);
    let seed = 0x9e37_79b9_7f4a_7c15u64.wrapping_mul(case + 1);
    let source = generated_module(seed).expect("should generate");
    println!(
        "--- source ---\n{}",
        wasmprinter::print_bytes(&source).expect("should print")
    );
    let module = canon::instrument(&source, Config::default()).expect("instrument");

    println!(
        "--- instrumented ---\n{}",
        wasmprinter::print_bytes(&module).expect("should print")
    );

    let image = image::decode(&module).expect("decode");
    let mut machine = Machine::new(&image, Vec::new(), Limits::default()).expect("instantiate");
    loop {
        let at = machine.commit().program_counter;
        match machine.step() {
            Ok(cairn_runtime::engine::machine::Progress::Finished) => {
                println!("cairn: finished at {at:?}");
                break;
            }
            Ok(_) => println!("  step {:>4} at {at:?}", machine.steps()),
            Err(trap) => {
                println!("cairn: TRAP {trap:?} at {at:?} (step {})", machine.steps());
                break;
            }
        }
    }
    println!("wasmi:    {:?}", run_wasmi(&module, &[]));
    println!("wasmtime: {:?}", run_wasmtime(&module, &[]));
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

// --- the math library ------------------------------------------------------------------------

/// Builds `workloads/rust/math-probe` to WebAssembly, or `None` if the target is not installed.
///
/// Two `rustc` invocations rather than a nested cargo: `cairn-math` has no dependencies, so
/// there is nothing to resolve, and a cargo inside a cargo contends for the same build lock.
/// Where `rustc` is, given that a test does not inherit a build script's environment.
fn rustc_path() -> std::path::PathBuf {
    std::env::var("RUSTC")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            let beside = std::env::var("CARGO").ok().map(|cargo| {
                std::path::Path::new(&cargo).with_file_name(if cfg!(windows) {
                    "rustc.exe"
                } else {
                    "rustc"
                })
            });
            beside
                .filter(|path| path.exists())
                .unwrap_or_else(|| "rustc".into())
        })
}

/// The flags `docs/WORKLOADS.md` tells a workload author to pass, for the reasons it gives.
///
/// The memory maximum is required for admission and no toolchain emits one, and the shadow stack
/// has to be shrunk in the same breath or the link fails with a message that never mentions
/// stacks. The optimisation flags are worth a hundredfold in module size — see
/// `cairn-math/tests/wasm.rs`.
const fn wasm_flags() -> [&'static str; 20] {
    [
        "--edition",
        "2021",
        "--target",
        "wasm32-unknown-unknown",
        "-C",
        "opt-level=3",
        "-C",
        "panic=abort",
        "-C",
        "lto=fat",
        "-C",
        "codegen-units=1",
        "-C",
        "strip=symbols",
        "-C",
        "link-arg=-zstack-size=131072",
        "-C",
        "link-arg=--initial-memory=262144",
        "-C",
        "link-arg=--max-memory=262144",
    ]
}

/// Build one dependency-free `.rs` file into a workload module.
///
/// `None` means the WebAssembly target is not installed, which is a skip rather than a failure —
/// and only that reason, because a test that skips for an unexpected reason is worse than one
/// that fails.
fn build_probe(source: &str, crate_name: &str) -> Option<Vec<u8>> {
    let workloads = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../workloads/rust");
    let out = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(crate_name);
    std::fs::create_dir_all(&out).expect("should be able to write under the target dir");
    let wasm = out.join(format!("{crate_name}.wasm"));

    let built = std::process::Command::new(rustc_path())
        .args(wasm_flags())
        .args(["--crate-type", "cdylib", "--crate-name", crate_name])
        .arg("-o")
        .arg(&wasm)
        .arg(workloads.join(source))
        .output()
        .ok()?;
    if !built.status.success() {
        let complaint = String::from_utf8_lossy(&built.stderr);
        assert!(
            complaint.contains("wasm32-unknown-unknown"),
            "building {source} for WebAssembly failed for a reason other than a missing \
             target:\n{complaint}"
        );
        return None;
    }
    Some(std::fs::read(&wasm).expect("the probe module was not written"))
}

fn math_probe_module() -> Option<Vec<u8>> {
    let rustc = rustc_path();

    let workloads = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../workloads/rust");
    let out = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("math-probe");
    std::fs::create_dir_all(&out).expect("should be able to write under the target dir");
    let rlib = out.join("libcairn_math.rlib");

    let common = wasm_flags();

    let built = std::process::Command::new(&rustc)
        .args(common)
        .args(["--crate-type", "rlib", "--crate-name", "cairn_math"])
        .arg("-o")
        .arg(&rlib)
        .arg(workloads.join("cairn-math/src/lib.rs"))
        .output()
        .ok()?;
    if !built.status.success() {
        // A missing target is the expected reason and is not a failure; anything else is
        // reported, because a silently skipped test is worse than a failing one.
        let complaint = String::from_utf8_lossy(&built.stderr);
        assert!(
            complaint.contains("wasm32-unknown-unknown"),
            "building cairn-math for WebAssembly failed for a reason other than a missing \
             target:\n{complaint}"
        );
        return None;
    }

    let wasm = out.join("math-probe.wasm");
    let built = std::process::Command::new(&rustc)
        .args(common)
        .args(["--crate-type", "cdylib", "--crate-name", "math_probe"])
        .arg("--extern")
        .arg(format!("cairn_math={}", rlib.display()))
        .arg("-o")
        .arg(&wasm)
        .arg(workloads.join("math-probe/probe.rs"))
        .output()
        .ok()?;
    assert!(
        built.status.success(),
        "building the math probe failed:\n{}",
        String::from_utf8_lossy(&built.stderr)
    );
    Some(std::fs::read(&wasm).expect("the probe module was not written"))
}

/// Arguments for the math probe, in batches of eight.
///
/// Chosen for the places a transcendental function can behave differently rather than for
/// coverage of the number line: the named values whose results are specified, the boundaries
/// each function branches on, and — for the trigonometric functions — magnitudes far past where
/// a limited-precision argument reduction stops working.
fn math_arguments() -> Vec<Vec<u8>> {
    let mut values = vec![
        0.0,
        -0.0,
        1.0,
        -1.0,
        0.5,
        -0.5,
        2.0,
        -2.0,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NAN,
        // A NaN with a payload that is not the canonical one, which is what makes the
        // canonicalization rules observable.
        f64::from_bits(0x7ff8_0000_0000_0001),
        f64::from_bits(0xfff8_0000_0000_0001),
        f64::MIN_POSITIVE,
        -f64::MIN_POSITIVE,
        f64::MAX,
        f64::MIN,
        // Subnormals, which several functions have a separate path for.
        f64::from_bits(1),
        f64::from_bits(0x8000_0000_0000_0001),
        f64::from_bits(0x000f_ffff_ffff_ffff),
        // The constants the reductions branch around.
        std::f64::consts::PI,
        std::f64::consts::FRAC_PI_2,
        std::f64::consts::FRAC_PI_4,
        std::f64::consts::LN_2,
        std::f64::consts::E,
        // The worst case for argument reduction in this format: `x * (2/pi)` sits within
        // 2^-61 of an integer here, so the reduction has to carry over a hundred bits.
        6_381_956_970_095_103.0 * f64::from_bits(((1023 + 797) as u64) << 52),
        // Overflow and underflow thresholds for exp.
        709.782_712_893_383_9,
        -745.133_219_101_941_1,
        710.0,
        -746.0,
    ];

    // And a seeded spread across the whole exponent range, so the corpus is not only the cases
    // someone thought of.
    let mut rng = Rng(0xfeed_face_0bad_c0de);
    for _ in 0..200 {
        let exponent = (rng.next() % 2041) as i64 - 1020;
        let mantissa = rng.next() & ((1 << 52) - 1);
        let sign = (rng.next() & 1) << 63;
        values.push(f64::from_bits(
            sign | (((exponent + 1023) as u64) << 52) | mantissa,
        ));
    }

    values
        .chunks(8)
        .map(|batch| {
            batch
                .iter()
                .flat_map(|v| v.to_bits().to_le_bytes())
                .collect()
        })
        .collect()
}

/// The property [`cairn-math`](../../workloads/rust/cairn-math/src/lib.rs) exists for.
///
/// WebAssembly has no `exp`, no `log` and no `sin`, so a workload that needs them must either
/// import them from the host or compile them in. Importing them is the obvious choice and it
/// would break Cairn: measured over twenty thousand inputs, V8 and the platform libm this test
/// runs on disagree on **every one** of twelve functions, and on `cbrt` they disagree on 29.8%
/// of them. Under Cairn's rules a disagreement is not a rounding difference — it is a dispute,
/// and arbitration would convict whichever honest volunteer was on the engine that lost.
///
/// So the math is compiled into the module, out of nothing but the arithmetic WebAssembly
/// specifies exactly. This test is the evidence that it worked. It is the only case in this
/// file compiled from Rust by a real toolchain rather than written in the text format, which
/// was for a while the only one that checked Cairn admits what a stock toolchain emits.
///
/// **That claim was too strong and ADR-0018 is what it cost.** `cairn-math` is statically
/// resolved arithmetic from end to end, so it contains no indirect call and never touches a
/// function table — and the encoding Cairn was refusing lives in `call_indirect`. This test
/// passed throughout, while every workload with a trait object in it was rejected at the gate.
/// [`the_shapes_a_compiler_turns_into_indirect_calls_agree_on_every_engine`] is the other half.
#[test]
fn the_math_library_computes_the_same_bits_on_every_engine() {
    let Some(source) = math_probe_module() else {
        assert!(
            !std::env::var("CAIRN_REQUIRE_WASM").is_ok_and(|required| required == "1"),
            "CAIRN_REQUIRE_WASM=1 but wasm32-unknown-unknown is not installed"
        );
        println!(
            "SKIPPED: wasm32-unknown-unknown is not installed, so the math library was NOT \
             checked across engines. Install it with `rustup target add \
             wasm32-unknown-unknown`, or set CAIRN_REQUIRE_WASM=1 to make this a failure."
        );
        return;
    };

    let arguments = math_arguments();
    let mut checked = 0u32;
    for (which, config) in [
        ("dispute", Config::default()),
        ("honest", HONEST_CONFIGS[0]),
    ] {
        let module = canonical_bytes(&source, config);
        for (index, input) in arguments.iter().enumerate() {
            let name = format!("math {which} batch {index}");
            let mine = run_cairn(&module, input);
            if mine.hit_a_cairn_limit {
                continue;
            }
            assert!(
                mine.output.is_some(),
                "{name}: the math library trapped, which it has no business doing"
            );

            let interpreted = run_wasmi(&module, input);
            assert_eq!(
                mine.fuel, interpreted.fuel,
                "{name}: fuel differs against wasmi"
            );
            assert_eq!(
                mine.output, interpreted.output,
                "{name}: output differs against wasmi"
            );

            let compiled = run_wasmtime(&module, input);
            assert_eq!(
                mine.fuel, compiled.fuel,
                "{name}: fuel differs against wasmtime"
            );
            assert_eq!(
                mine.output, compiled.output,
                "{name}: output differs against wasmtime"
            );
            checked += 1;
        }
    }
    assert!(checked > 50, "only {checked} math batches were compared");
    println!("{checked} batches of eight arguments agreed across three engines");
}

/// A compiler's indirect calls go through the gate and every engine, and this is why.
///
/// # The measurement this test exists to hold in place
///
/// Cairn refused this workload until [ADR-0018], and not for anything the workload does. `rustc`
/// writes `call_indirect`'s table index as a **padded five-byte LEB128**, `80 80 80 80 00`, where
/// the base specification wants a single zero byte — a spelling the reference-types proposal
/// permits and the base specification does not. So the admission gate answered
/// `zero byte expected` for a module with one table, table index zero, and no reference anywhere
/// near a value.
///
/// **That excluded most non-trivial compiler output**: a trait object, a function pointer, or any
/// `dyn` dispatch at all is enough to produce one. It went unnoticed because `math-probe`, the
/// only compiled workload in the repository, contains no indirect call — its functions are all
/// statically resolved arithmetic, which is exactly the shape that does not exercise a table.
///
/// So the check is not "does the gate accept reference types". It is: **take a real toolchain's
/// output for the constructs a compiler cannot avoid lowering through a table, and require every
/// engine to agree on it.**
///
/// [ADR-0018]: ../../docs/adr/0018-a-compilers-call-indirect-is-not-the-specifications.md
#[test]
fn the_shapes_a_compiler_turns_into_indirect_calls_agree_on_every_engine() {
    let Some(source) = build_probe("dispatch-probe/probe.rs", "dispatch_probe") else {
        assert!(
            !std::env::var("CAIRN_REQUIRE_WASM").is_ok_and(|required| required == "1"),
            "CAIRN_REQUIRE_WASM=1 but wasm32-unknown-unknown is not installed"
        );
        println!(
            "SKIPPED: wasm32-unknown-unknown is not installed, so a real toolchain's \
             `call_indirect` was NOT checked. Install it with `rustup target add \
             wasm32-unknown-unknown`, or set CAIRN_REQUIRE_WASM=1 to make this a failure."
        );
        return;
    };

    // First the claim about the gate, stated against the submitted bytes rather than the
    // canonical ones. `canonical_bytes` would refuse them too, but through a panic that says
    // nothing about which rule refused what.
    validate::validate_submitted(&source, validate::Limits::default()).unwrap_or_else(|refusal| {
        panic!(
            "a stock `rustc` produced a workload Cairn will not admit: {refusal}\n\
             This is the failure ADR-0018 is about. Do not widen the gate to make it pass \
             without checking what the toolchain started emitting and whether the interpreter \
             implements it."
        )
    });

    // And that the padded spelling is really in there. Without this the test would keep passing
    // if a future toolchain switched to the single-byte form — still useful, but no longer
    // evidence about the thing ADR-0018 is about, and silently so.
    //
    // `0x11`, then the type index as a LEB128, then the table index as a LEB128. The table index
    // is the one the base specification insists is a lone `0x00`; a length above one byte is the
    // spelling that was being refused. Scanning for the bytes rather than decoding the section
    // properly is enough here: a false positive would have to be a `0x11` inside a constant
    // followed by two well-formed LEBs, and the count below is what would catch that.
    let leb_len = |at: usize| -> Option<usize> {
        let mut n = 0;
        while let Some(byte) = source.get(at + n) {
            n += 1;
            if byte & 0x80 == 0 {
                return Some(n);
            }
            if n > 5 {
                return None;
            }
        }
        None
    };
    let mut indirect = 0u32;
    let mut padded = 0u32;
    for at in 0..source.len() {
        if source.get(at) != Some(&0x11) {
            continue;
        }
        let Some(type_len) = leb_len(at + 1) else {
            continue;
        };
        let Some(table_len) = leb_len(at + 1 + type_len) else {
            continue;
        };
        indirect += 1;
        if table_len > 1 {
            padded += 1;
        }
    }
    assert!(
        indirect > 0,
        "the probe compiled without a single `call_indirect`, so it is testing nothing — the \
         optimiser devirtualised the trait objects and function pointers this file is made of"
    );
    assert!(
        padded > 0,
        "no `call_indirect` carries a multi-byte table index, so this toolchain no longer emits \
         the encoding ADR-0018 is about. That is not a failure of Cairn — but this test is no \
         longer evidence for the decision, and the ADR should say so."
    );
    println!(
        "{padded} of {indirect} call_indirect immediates use the padded encoding the base \
         specification forbids"
    );

    let mut checked = 0u32;
    for (which, config) in [
        ("dispute", Config::default()),
        ("honest", HONEST_CONFIGS[0]),
    ] {
        let module = canonical_bytes(&source, config);
        for input in [
            &b""[..],
            &b"a"[..],
            &b"alpha"[..],
            &b"the quick brown fox jumps over the lazy dog"[..],
            &[0u8; 200][..],
            &[0xffu8; 255][..],
        ] {
            let name = format!("dispatch {which} on {} bytes", input.len());
            let mine = run_cairn(&module, input);
            if mine.hit_a_cairn_limit {
                continue;
            }
            assert!(
                mine.output.is_some(),
                "{name}: the probe trapped, which it has no business doing"
            );

            let interpreted = run_wasmi(&module, input);
            assert_eq!(
                mine.fuel, interpreted.fuel,
                "{name}: fuel differs against wasmi"
            );
            assert_eq!(
                mine.output, interpreted.output,
                "{name}: output differs against wasmi"
            );

            let compiled = run_wasmtime(&module, input);
            assert_eq!(
                mine.fuel, compiled.fuel,
                "{name}: fuel differs against wasmtime"
            );
            assert_eq!(
                mine.output, compiled.output,
                "{name}: output differs against wasmtime"
            );
            checked += 1;
        }
    }
    assert!(checked >= 12, "only {checked} dispatch cases were compared");
    println!("{checked} indirect-dispatch cases agreed across three engines");
}

/// Build `workloads/template` the way its README tells an author to.
///
/// **With `cargo`, not `rustc`, and that is the point of it.** The template's value is that
/// somebody can copy the directory and type `cargo build --release`; the manifest, the profile
/// and above all `.cargo/config.toml` are what make that work, and none of them is exercised by
/// invoking `rustc` with the flags spelled out. This is the only test in the repository that
/// checks the instructions rather than the code.
///
/// Nesting cargo inside cargo is safe here only because the template declares its own
/// `[workspace]` and therefore builds into its own `target/`, so it takes a different build lock.
/// `cairn-math/tests/wasm.rs` avoids nesting for exactly the reason that does not apply here.
fn build_with_cargo(directory: &str, artefact: &str) -> Option<Vec<u8>> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let package = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../workloads")
        .join(directory);

    let built = std::process::Command::new(cargo)
        .arg("build")
        .arg("--release")
        .arg("--manifest-path")
        .arg(package.join("Cargo.toml"))
        // Cargo passes its own target directory down through the environment, and inheriting it
        // would put these artefacts in the workspace's `target/` — taking the lock this test is
        // running under, and deadlocking. The package's own directory is the default.
        .env_remove("CARGO_TARGET_DIR")
        .output()
        .ok()?;
    if !built.status.success() {
        let complaint = String::from_utf8_lossy(&built.stderr);
        assert!(
            complaint.contains("wasm32-unknown-unknown"),
            "workloads/{directory} does not build, which is the one thing it exists to do:\n\
             {complaint}"
        );
        return None;
    }

    let wasm = package
        .join("target/wasm32-unknown-unknown/release")
        .join(artefact);
    Some(
        std::fs::read(&wasm)
            .unwrap_or_else(|e| panic!("{directory} built but produced no module: {e}")),
    )
}

fn build_template() -> Option<Vec<u8>> {
    build_with_cargo("template", "cairn_workload_template.wasm")
}

/// Every `(module, name)` the given module imports.
fn imports_of(module: &[u8]) -> Vec<String> {
    let mut found = Vec::new();
    for payload in wasmparser::Parser::new(0).parse_all(module) {
        if let Ok(wasmparser::Payload::ImportSection(reader)) = payload {
            // `into_imports` rather than `into_iter`, for the reason `validate.rs` gives: it
            // flattens the compact-import encoding, so this stays correct if the allowlist grows.
            for import in reader.into_imports().flatten() {
                found.push(format!("{}.{}", import.module, import.name));
            }
        }
    }
    found.sort();
    found
}

/// The workload template compiles, is admitted, imports nothing, and agrees everywhere.
///
/// # Why this test is the acceptance criterion for the SDK
///
/// The roadmap's test for a workload SDK was *"somebody who is not me compiles a workload"*, and
/// this is as close as an automated check gets: it runs the documented command against the
/// documented directory and requires the result to be a module Cairn will actually take.
///
/// # The import assertion is doing more work than it looks like
///
/// `workloads/template` is a **`std`** crate, which reads as a mistake if you have absorbed the
/// usual advice about `no_std` on a bare target. An earlier draft was `no_std`, and it could not
/// depend on `cairn-math`: `f64::sqrt`, `floor`, `ceil`, `trunc` and `round_ties_even` are single
/// WebAssembly instructions that Rust puts in `std` rather than `core`, so a `no_std` workload
/// using that library fails with `found duplicate lang item panic_impl`.
///
/// **`no_std` was a proxy for "nothing comes from the host", and this is the property itself.**
/// If `std` ever dragged an allocator hook, a clock or a libm call into the module, it would
/// appear here as a third import. See ADR-0019.
#[test]
fn the_workload_template_compiles_and_is_admissible() {
    let Some(source) = build_template() else {
        assert!(
            !std::env::var("CAIRN_REQUIRE_WASM").is_ok_and(|required| required == "1"),
            "CAIRN_REQUIRE_WASM=1 but wasm32-unknown-unknown is not installed"
        );
        println!(
            "SKIPPED: wasm32-unknown-unknown is not installed, so the workload template was NOT \
             built. Install it with `rustup target add wasm32-unknown-unknown`, or set \
             CAIRN_REQUIRE_WASM=1 to make this a failure."
        );
        return;
    };

    validate::validate_submitted(&source, validate::Limits::default()).unwrap_or_else(|refusal| {
        panic!(
            "the workload template produces a module Cairn will not admit: {refusal}\n\
             Everything an author is told to do is in that directory, so this is a broken \
             instruction rather than a broken workload."
        )
    });

    assert_eq!(
        imports_of(&source),
        vec!["cairn.input".to_owned(), "cairn.output".to_owned()],
        "the template imports something other than Cairn's two host functions"
    );

    let mut checked = 0u32;
    for (which, config) in [
        ("dispute", Config::default()),
        ("honest", HONEST_CONFIGS[0]),
    ] {
        let module = canonical_bytes(&source, config);
        for input in [&b""[..], &b"alpha"[..], &[0xffu8; 300][..]] {
            let name = format!("template {which} on {} bytes", input.len());
            let mine = run_cairn(&module, input);
            if mine.hit_a_cairn_limit {
                continue;
            }
            assert!(mine.output.is_some(), "{name}: the template trapped");

            let interpreted = run_wasmi(&module, input);
            assert_eq!(mine.fuel, interpreted.fuel, "{name}: fuel differs vs wasmi");
            assert_eq!(
                mine.output, interpreted.output,
                "{name}: output differs vs wasmi"
            );

            let compiled = run_wasmtime(&module, input);
            assert_eq!(mine.fuel, compiled.fuel, "{name}: fuel differs vs wasmtime");
            assert_eq!(
                mine.output, compiled.output,
                "{name}: output differs vs wasmtime"
            );
            checked += 1;
        }
    }
    assert!(checked >= 6, "only {checked} template cases were compared");
    println!(
        "the template is {} bytes, imports exactly its two host functions, and agreed on \
         {checked} cases across three engines",
        source.len()
    );
}

// --- a real scientific kernel ------------------------------------------------------------------

/// A time series with a periodic signal in it, at *uneven* intervals.
///
/// Uneven on purpose: even sampling is what a periodogram is not needed for. A telescope observes
/// when the weather, the daylight and the schedule allow, and Lomb–Scargle exists because the
/// Fourier transform assumes a regular grid that real observations do not lie on.
///
/// The jitter comes from a small multiplicative congruential generator rather than a random number
/// crate, so the series is a pure function of `seed` and this test asserts things about a signal
/// it can regenerate exactly. Nothing here needs statistical quality; it needs reproducibility.
fn synthetic_observations(count: usize, frequency: f64, seed: u64) -> (Vec<f64>, Vec<f64>) {
    let mut state = seed | 1;
    let mut next_unit = || {
        // Lehmer, with a well-known multiplier. The top bits are the good ones.
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        ((state >> 11) as f64) / ((1u64 << 53) as f64)
    };

    let mut times = Vec::with_capacity(count);
    let mut values = Vec::with_capacity(count);
    let mut clock = 0.0f64;
    for _ in 0..count {
        // A gap of between 0.2 and 1.2 days, so the series is irregular but never goes backwards.
        clock += 0.2 + next_unit();
        let phase = std::f64::consts::TAU * frequency * clock + 0.7;
        // Signal plus a little noise, so the peak is real rather than an artefact of a noiseless
        // sinusoid landing exactly on a sampled frequency.
        let noise = (next_unit() - 0.5) * 0.20;
        times.push(clock);
        values.push(12.0 + 0.5 * phase.sin() + noise);
    }
    (times, values)
}

/// Pack a unit for the periodogram workload. The layout is in its own crate documentation.
fn periodogram_input(times: &[f64], values: &[f64], band: (f64, f64), frequencies: u32) -> Vec<u8> {
    let mut unit = Vec::with_capacity(24 + times.len() * 16);
    unit.extend_from_slice(&band.0.to_le_bytes());
    unit.extend_from_slice(&band.1.to_le_bytes());
    unit.extend_from_slice(&frequencies.to_le_bytes());
    unit.extend_from_slice(&(times.len() as u32).to_le_bytes());
    for (time, value) in times.iter().zip(values) {
        unit.extend_from_slice(&time.to_le_bytes());
        unit.extend_from_slice(&value.to_le_bytes());
    }
    unit
}

/// The three `f64`s a periodogram unit answers with.
fn periodogram_output(bytes: &[u8]) -> (f64, f64, f64) {
    assert_eq!(
        bytes.len(),
        24,
        "a periodogram unit answers with three f64s"
    );
    let read = |at: usize| {
        let mut word = [0u8; 8];
        word.copy_from_slice(&bytes[at..at + 8]);
        f64::from_le_bytes(word)
    };
    (read(0), read(8), read(16))
}

/// A real scientific kernel: recover a known period, and agree about it on every engine.
///
/// # Why this test is different from every other one in this file
///
/// Everything else here checks that engines **agree**. Agreement is necessary and it is not
/// sufficient: three engines computing the same wrong number agree perfectly. This one also checks
/// that the answer is **right**, against something outside the computation — a signal synthesised
/// at a known frequency, which the periodogram has to find.
///
/// That is what made the roadmap put a real workload after the math library rather than before it.
/// A genuine numerical kernel is exactly where a float divergence hides, so a workload built on
/// host trigonometry would have produced a mystery: a dispute rate nobody could explain, on a
/// computation nobody could check. With [ADR-0016](../../docs/adr/0016-math-belongs-in-the-module-not-the-host.md)
/// the trigonometry is in the module, and both halves can be asserted at once.
#[test]
fn a_periodogram_recovers_a_known_period_and_every_engine_agrees() {
    let Some(source) = build_with_cargo("periodogram", "cairn_periodogram.wasm") else {
        assert!(
            !std::env::var("CAIRN_REQUIRE_WASM").is_ok_and(|required| required == "1"),
            "CAIRN_REQUIRE_WASM=1 but wasm32-unknown-unknown is not installed"
        );
        println!(
            "SKIPPED: wasm32-unknown-unknown is not installed, so the scientific kernel was NOT \
             run. Install it with `rustup target add wasm32-unknown-unknown`, or set \
             CAIRN_REQUIRE_WASM=1 to make this a failure."
        );
        return;
    };

    validate::validate_submitted(&source, validate::Limits::default())
        .unwrap_or_else(|refusal| panic!("the periodogram workload is not admissible: {refusal}"));

    // 0.137 cycles per day — about a 7.3-day period, and deliberately not a round number or a
    // multiple of the sampling, so a peak there cannot be an artefact of either.
    const TRUE_FREQUENCY: f64 = 0.137;
    const BAND: (f64, f64) = (0.05, 0.30);

    // **Sized for a test, not for production.** Every (observation, frequency) pair costs four
    // transcendental calls, and this runs under Cairn's interpreter on the dispute path — the
    // slowest configuration there is. 120 × 400 is a few hundred thousand calls and a couple of
    // seconds; a real unit would be tens of times larger and would still be a fraction of a second
    // on the compiled engine a volunteer actually uses.
    const OBSERVATIONS: usize = 120;
    const FREQUENCIES: u32 = 400;

    let (times, values) = synthetic_observations(OBSERVATIONS, TRUE_FREQUENCY, 0x5eed_1234);
    let span = times.last().expect("observations") - times.first().expect("observations");
    let unit = periodogram_input(&times, &values, BAND, FREQUENCIES);

    // The two committed input files, which are the same observations over two different bands —
    // which is how a real search is split among volunteers. They exist so the workload's README
    // commands run and so a coordinator demonstration has genuine units to hand out.
    //
    // **Checked rather than merely shipped.** A committed binary nobody can regenerate is exactly
    // what `cairn-math/tests/wasm.rs` refuses to have, so these are regenerated here and compared:
    // if the generator above changes, this fails and says how to refresh them. One source of
    // truth, and it is the code that also asserts the science.
    let workload =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../workloads/periodogram");
    let regenerate = std::env::var("CAIRN_REGENERATE_INPUTS").is_ok_and(|value| value == "1");
    for (name, band) in [
        ("band-with-the-signal.bin", BAND),
        ("band-without-it.bin", (0.30, 0.55)),
    ] {
        let expected = periodogram_input(&times, &values, band, FREQUENCIES);
        let path = workload.join(name);
        if regenerate {
            std::fs::write(&path, &expected)
                .unwrap_or_else(|e| panic!("could not write {name}: {e}"));
            println!("regenerated {name} ({} bytes)", expected.len());
            continue;
        }
        let committed = std::fs::read(&path).unwrap_or_else(|e| {
            panic!(
                "{name} is missing: {e}. Regenerate the committed inputs \
                with CAIRN_REGENERATE_INPUTS=1"
            )
        });
        assert_eq!(
            committed, expected,
            "{name} no longer matches what this test generates. If that was intended, refresh it \
             with CAIRN_REGENERATE_INPUTS=1"
        );
    }
    if regenerate {
        // Said loudly, because this test passing while having checked nothing is the shape of
        // thing that goes unnoticed for months. A silently skipped test is worse than a failing
        // one, which is the same reason `CAIRN_REQUIRE_WASM` exists.
        println!(
            "CAIRN_REGENERATE_INPUTS=1: the committed units were rewritten and NOTHING ELSE WAS \
             CHECKED. Run this test again without it to verify the science."
        );
        return;
    }

    let module = canonical_bytes(&source, Config::default());
    let mine = run_cairn(&module, &unit);
    assert!(
        !mine.hit_a_cairn_limit,
        "the periodogram unit exceeded an interpreter limit"
    );
    let answer = mine
        .output
        .clone()
        .expect("the periodogram trapped, which it has no business doing");

    // Half of the test: every engine agrees, bit for bit, on both the answer and the work done.
    let interpreted = run_wasmi(&module, &unit);
    assert_eq!(mine.fuel, interpreted.fuel, "fuel differs against wasmi");
    assert_eq!(
        mine.output, interpreted.output,
        "output differs against wasmi"
    );
    let compiled = run_wasmtime(&module, &unit);
    assert_eq!(mine.fuel, compiled.fuel, "fuel differs against wasmtime");
    assert_eq!(
        mine.output, compiled.output,
        "output differs against wasmtime"
    );

    // The other half, and the one agreement cannot give: the science is right.
    let (peak, power, total) = periodogram_output(&answer);

    // **The Rayleigh resolution is the honest bound.** A periodogram cannot separate frequencies
    // closer together than `1/T`, where `T` is the observing span, however finely the band is
    // sampled — so that, and not the bin width, is what the recovered peak has to fall inside.
    // Asserting anything tighter would be asserting a property of this particular noise draw.
    let resolution = 1.0 / span;
    let error = (peak - TRUE_FREQUENCY).abs();
    assert!(
        error < resolution,
        "the peak came back at {peak:.6} c/d against a true {TRUE_FREQUENCY} c/d — off by \
         {error:.6}, which is more than the {resolution:.6} c/d this series can resolve"
    );

    // And the peak has to be a *detection* rather than the largest value in a flat band.
    //
    // **Thresholded on the power itself, not on its ratio to the band mean.** The first version of
    // this test asserted `power > 20 × mean` and failed at 19.8, which is the sort of number that
    // gets quietly relaxed until it passes. The ratio was the wrong statistic: this workload
    // normalises by the variance, so under pure noise the power at each independent frequency is
    // exponentially distributed with mean 1 — and the band mean here is not 1 but ~3, because the
    // peak is about nineteen bins wide (the Rayleigh width divided by the frequency step) and its
    // own shoulders are most of what raises it. Dividing by a mean the signal inflates measures
    // the signal against itself.
    //
    // A power of 20 has a false-alarm probability of e⁻²⁰ ≈ 2×10⁻⁹ per independent frequency, so
    // across this band it is around 10⁻⁶. That is a threshold with a meaning rather than a
    // threshold that happens to pass.
    let mean_power = total / f64::from(FREQUENCIES);
    assert!(
        power > 20.0,
        "peak power {power:.2} is below the detection threshold, so nothing was found and the \
         frequency above is meaningless (band mean {mean_power:.2})"
    );

    println!(
        "recovered {peak:.6} c/d against a true {TRUE_FREQUENCY} c/d \
         ({error:.6} off, resolution {resolution:.6}), power {power:.1} against a band mean of \
         {mean_power:.2}, over {} observations spanning {span:.1} days — identical bytes and \
         {} fuel on three engines",
        times.len(),
        mine.fuel,
    );
}

// --- the fourth engine: the one volunteers actually use ---------------------------------------

/// One case handed to the browser's engine, with what Cairn made of it kept on this side.
struct BrowserCase {
    name: String,
    module: Vec<u8>,
    input: Vec<u8>,
    expected: Outcome,
}

/// Is `node` on the path?
///
/// Checked rather than assumed, because a Rust contributor with no Node installed should still
/// be able to run `cargo test` — and because a test that pretends to have run is worse than one
/// that says it did not. See the skip handling in
/// [`the_browsers_own_engine_agrees_with_cairn`].
fn node_is_available() -> bool {
    std::process::Command::new("node")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

fn from_hex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

/// Every case worth handing to a browser, built from the two generators.
///
/// The hand-written corpus is deliberately absent. It covers instruction semantics a mature
/// engine is not going to get wrong in a way the generated modules would miss, and keeping this
/// to the generators is what makes the corpus large enough to be worth a process boundary. What
/// the generators cover is exactly the two things a *different* engine could plausibly differ
/// on:
///
/// - **Float expressions under the honest configurations.** The highest-value case in the file.
///   `Canonicalization::AtEscapes` is the newest and least-proven reasoning in this repository
///   ([ADR-0006]), the browser runs the honest path in production, and V8 is a JIT — so constant
///   folding, fused multiply-add and reassociation are available to it and to no engine already
///   in this gate except wasmtime.
/// - **Whole generated modules under full instrumentation**, for control flow, memory, calls and
///   the combinations nobody would write down. Full instrumentation only, for the reason
///   [`generated_modules_agree_across_engines`] gives: a generated module halts because
///   `ensure_termination` injected a counter, and pairing that with the honest path would
///   compare two different termination stories.
///
/// [ADR-0006]: ../../docs/adr/0006-canonicalize-nans-at-escapes-on-the-honest-path.md
fn browser_corpus() -> Vec<BrowserCase> {
    const FLOAT_CASES: u32 = 300;
    const MODULE_CASES: u64 = 200;

    let mut corpus = Vec::new();

    // The same seed as `random_float_expressions_agree_across_engines`, so both tests look at
    // the same expressions and a failure here can be reproduced there.
    let mut rng = Rng(0x5eed_1234_abcd_ef01);
    for case in 0..FLOAT_CASES {
        let seed = rng.0;
        let depth = 2 + (case % 3);
        let expression = random_f64(&mut rng, depth);
        let escape = match case % 3 {
            0 => format!("(f64.store (i32.const 0) {expression})"),
            1 => format!("(i64.store (i32.const 0) (i64.reinterpret_f64 {expression}))"),
            _ => format!("(f64.store (i32.const 0) (f64.copysign (f64.const 1.5) {expression}))"),
        };
        let text = format!(
            r#"(module
                 (import "cairn" "output" (func $output (param i32 i32)))
                 (memory (export "memory") 1 8)
                 (global $g (mut f64) (f64.const 0))
                 (func (export "cairn_run") (local $x f64)
                   (local.set $x (f64.reinterpret_i64 (i64.const 0x7ff8000000000005)))
                   (global.set $g {expression})
                   {escape}
                   (call $output (i32.const 0) (i32.const 8))))"#
        );

        let configs = [
            ("dispute", Config::default()),
            ("honest", HONEST_CONFIGS[0]),
            ("honest-everywhere", HONEST_CONFIGS[1]),
        ];
        for (which, config) in configs {
            let module = canonical_with(&text, config);
            let expected = run_cairn(&module, &[]);
            if expected.hit_a_cairn_limit {
                continue;
            }
            corpus.push(BrowserCase {
                name: format!("float case {case} ({which}, seed {seed:#x})"),
                module,
                input: Vec::new(),
                expected,
            });
        }
    }

    for case in 0..MODULE_CASES {
        let seed = 0x9e37_79b9_7f4a_7c15u64.wrapping_mul(case + 1);
        let Some(source) = generated_module(seed) else {
            continue;
        };
        if validate::validate_submitted(&source, validate::Limits::default()).is_err() {
            continue;
        }
        let Ok(module) = canon::instrument(&source, Config::default()) else {
            continue;
        };
        let expected = run_cairn(&module, &[]);
        if expected.hit_a_cairn_limit {
            continue;
        }
        corpus.push(BrowserCase {
            name: format!("generated module {case} (seed {seed:#x})"),
            module,
            input: Vec::new(),
            expected,
        });
    }

    // And the math library, which is the reason the browser matters most of all. Every other
    // case here is arithmetic WebAssembly specifies exactly, where agreement is close to
    // guaranteed. `cairn-math` is arithmetic the *specification says nothing about* — `exp`,
    // `log`, `sin`, `pow` — reconstructed out of the operations it does specify. The measured
    // reason for doing that is that V8 and the platform libm disagree on every one of twelve
    // such functions, so V8 is precisely the engine this has to be checked against.
    //
    // Only the leading batches are sent. They hold the named values, the boundaries each
    // function branches on, and the worst case for argument reduction; the rest are a random
    // spread that the three in-process engines already cover, and each case here costs a
    // fifteen-kilobyte file on disk.
    if let Some(source) = math_probe_module() {
        for (which, config) in [
            ("dispute", Config::default()),
            ("honest", HONEST_CONFIGS[0]),
        ] {
            let module = canonical_bytes(&source, config);
            for (index, input) in math_arguments().iter().take(8).enumerate() {
                let expected = run_cairn(&module, input);
                if expected.hit_a_cairn_limit {
                    continue;
                }
                corpus.push(BrowserCase {
                    name: format!("math batch {index} ({which})"),
                    module: module.clone(),
                    input: input.clone(),
                    expected,
                });
            }
        }
    }

    // And the real scientific kernel, which is where the math library is used the way a workload
    // actually uses it: thousands of `sin` and `cos` calls accumulated into running sums, rather
    // than one call per argument compared in isolation. **If a divergence survives everything
    // above, this is the shape it survives in** — and V8 is the engine that would be running it.
    //
    // Deliberately a small unit. Forty observations over sixty frequencies is under ten thousand
    // transcendental calls, which is nothing for any engine here, and the property under test is
    // agreement rather than throughput.
    if let Some(source) = build_with_cargo("periodogram", "cairn_periodogram.wasm") {
        let (times, values) = synthetic_observations(40, 0.137, 0x5eed_1234);
        let input = periodogram_input(&times, &values, (0.05, 0.30), 60);
        for (which, config) in [
            ("dispute", Config::default()),
            ("honest", HONEST_CONFIGS[0]),
        ] {
            let module = canonical_bytes(&source, config);
            let expected = run_cairn(&module, &input);
            if expected.hit_a_cairn_limit {
                continue;
            }
            corpus.push(BrowserCase {
                name: format!("periodogram ({which})"),
                module,
                input: input.clone(),
                expected,
            });
        }
    }

    corpus
}

/// The engine a volunteer actually runs, checked against Cairn's interpreter.
///
/// # Why this is worth a process boundary
///
/// Every other engine in this file is Rust, linked into this binary. **None of them is the
/// engine this project is for.** A volunteer opens a tab and the unit runs on V8 or
/// SpiderMonkey; until this test existed, that engine was the only one in the system that
/// nothing compared against anything.
///
/// The failure it guards is the project's worst shape. Cairn does not detect a wrong answer, it
/// detects a *disagreement*, and then decides which party was lying. A browser engine that
/// disagreed with Cairn's interpreter about some float expression would not be caught
/// cheating — it would be **convicted** of cheating, in a dispute it had no way to win, for the
/// offence of running in a browser. Silent, rare, concentrated on whichever engine lost, and
/// indistinguishable from fraud.
///
/// # What runs, and what does not
///
/// The corpus goes through `browser/differential.js`, which imports the volunteer's own
/// `host.js` — the same three host functions and the same fuel reading `worker.js` uses. It is
/// Node rather than a real browser, so this is V8 without the browser around it; a
/// headless-browser matrix across V8, SpiderMonkey and JavaScriptCore is the obvious next step
/// and needs a toolchain the browser worker deliberately does not have.
///
/// If `node` is absent the test **skips loudly** rather than passing, so a Rust contributor with
/// no Node installed can still run `cargo test`. `CAIRN_REQUIRE_NODE=1` turns the skip into a
/// failure; CI sets it, which is what stops this going quietly vacuous in the one place it must
/// not.
#[test]
fn the_browsers_own_engine_agrees_with_cairn() {
    if !node_is_available() {
        assert!(
            !std::env::var("CAIRN_REQUIRE_NODE").is_ok_and(|required| required == "1"),
            "CAIRN_REQUIRE_NODE=1 but `node` is not on the path"
        );
        println!(
            "SKIPPED: `node` is not on the path, so the browser's engine was NOT checked. That \
             is the engine volunteers actually run. Install Node, or set CAIRN_REQUIRE_NODE=1 \
             to make this a failure."
        );
        return;
    }

    let corpus = browser_corpus();
    assert!(
        corpus.len() > 500,
        "only {} cases reached the browser — the corpus has gone vacuous",
        corpus.len()
    );

    let directory = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("browser-differential");
    // Removed rather than reused: a stale `case-N.wasm` from a run with a different corpus size
    // would be compared against this run's expectations.
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("should be able to write under the target dir");

    let mut manifest = String::new();
    for (index, case) in corpus.iter().enumerate() {
        std::fs::write(directory.join(format!("case-{index}.wasm")), &case.module)
            .expect("should be able to write a case");
        manifest.push_str(&format!("{index}\t{}\n", to_hex(&case.input)));
    }
    std::fs::write(directory.join("manifest.tsv"), &manifest).expect("should be able to write");

    let harness = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("browser")
        .join("differential.js");
    let run = std::process::Command::new("node")
        .arg(&harness)
        .arg(&directory)
        .output()
        .expect("node should start");
    assert!(
        run.status.success(),
        "browser/differential.js failed:\n{}\n{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr),
    );

    let results = std::fs::read_to_string(directory.join("results.tsv"))
        .expect("the harness should have written results");
    let mut checked = 0u32;
    for line in results.lines().filter(|line| !line.trim().is_empty()) {
        let mut fields = line.split('\t');
        let index: usize = fields
            .next()
            .and_then(|field| field.parse().ok())
            .expect("a result line starts with a case index");
        let kind = fields.next().expect("a result line says ok or trap");
        let fuel: u64 = fields
            .next()
            .and_then(|field| field.parse().ok())
            .expect("a result line carries a fuel count");
        let output = from_hex(fields.next().unwrap_or("")).expect("output should be hex");

        let case = corpus
            .get(index)
            .expect("the harness invented a case index");
        let theirs = Outcome::reference(
            match kind {
                "ok" => Some(output),
                "trap" => None,
                other => panic!("unknown result kind {other:?}"),
            },
            fuel,
        );

        assert_eq!(
            case.expected.output.is_some(),
            theirs.output.is_some(),
            "{}: cairn and the browser's engine disagree about whether execution trapped",
            case.name,
        );
        assert_eq!(
            case.expected.fuel, theirs.fuel,
            "{}: fuel differs against the browser's engine, so it took a different path",
            case.name,
        );
        assert_eq!(
            case.expected.output, theirs.output,
            "{}: output differs against the browser's engine",
            case.name,
        );
        checked += 1;
    }

    assert_eq!(
        checked as usize,
        corpus.len(),
        "the harness reported {checked} of {} cases",
        corpus.len()
    );
    println!(
        "browser engine: {checked} units agreed with Cairn's interpreter — {}",
        String::from_utf8_lossy(&run.stdout).trim()
    );
}
