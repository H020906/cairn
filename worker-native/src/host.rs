//! The host side of the Cairn interface, for a volunteer's own engine.
//!
//! Three imports and nothing else. A workload can read its input, write its result, and — on
//! the dispute path only — report how far it has executed. It cannot reach a clock, entropy,
//! the filesystem or the network, because those imports are not offered and
//! [`cairn_runtime::validate`] rejects a module that asks for anything else.
//!
//! `cairn.charge` is linked here even though the honest path never calls it, so that the same
//! host works for a module instrumented either way. On the honest path the counter simply stays
//! at zero.

use cairn_runtime::validate;
use wasmtime::{Caller, Engine, Extern, Linker, Module, Store};

/// What the host holds on a workload's behalf.
#[derive(Default)]
struct Host {
    input: Vec<u8>,
    output: Vec<u8>,
    fuel: u64,
}

/// What one execution produced.
pub struct Executed {
    /// The bytes the workload wrote through `cairn.output`.
    pub output: Vec<u8>,
    /// How much work it was, if the unit was prepared to say.
    ///
    /// The interesting half of [ADR-0009]: a module metered through an exported counter runs on
    /// an engine Cairn does not control and still reports an exact, machine-independent
    /// instruction count. `None` means the module was not prepared to count, which is the
    /// default — there is no way to recover the number afterwards and no reason to want one.
    ///
    /// [ADR-0009]: ../../docs/adr/0009-metering-through-a-global-the-engines-disagree.md
    pub fuel: Option<u64>,
}

/// Compile and run `module` on wasmtime, returning what it wrote through `cairn.output`.
///
/// # Errors
///
/// Any failure to compile, link, instantiate or execute, as a message fit to print.
pub fn execute(module: &[u8], input: &[u8]) -> Result<Executed, String> {
    let engine = Engine::default();
    let module = Module::new(&engine, module).map_err(|e| format!("could not compile: {e}"))?;
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
            validate::HOST_MODULE,
            validate::HOST_CHARGE,
            |mut caller: Caller<'_, Host>, instructions: i32| {
                caller.data_mut().fuel += u64::from(instructions as u32);
            },
        )
        .map_err(|e| format!("could not link charge: {e}"))?;

    linker
        .func_wrap(
            validate::HOST_MODULE,
            validate::HOST_INPUT,
            |mut caller: Caller<'_, Host>, ptr: i32, len: i32| -> i32 {
                // Returns the full length available, whatever was asked for, so a workload can
                // size its buffer by calling with a length of zero first.
                let available = caller.data().input.len();
                let count = available.min(len as u32 as usize);
                if count > 0 {
                    let bytes = caller
                        .data()
                        .input
                        .get(..count)
                        .unwrap_or_default()
                        .to_vec();
                    if let Some(memory) = caller.get_export("memory").and_then(Extern::into_memory)
                    {
                        // A failed write means the workload named an out-of-bounds address. It
                        // will trap on its own shortly; the host does not need to judge.
                        let _ = memory.write(&mut caller, ptr as u32 as usize, &bytes);
                    }
                }
                available as i32
            },
        )
        .map_err(|e| format!("could not link input: {e}"))?;

    linker
        .func_wrap(
            validate::HOST_MODULE,
            validate::HOST_OUTPUT,
            |mut caller: Caller<'_, Host>, ptr: i32, len: i32| {
                let Some(memory) = caller.get_export("memory").and_then(Extern::into_memory) else {
                    return;
                };
                // Bounds-check *before* allocating. `len` is chosen by the workload, and this
                // host runs workloads a volunteer did not write: `vec![0u8; len]` on a length
                // of `0xffff_ffff` asks the operating system for four gigabytes, which is a
                // denial of service against the volunteer's own machine dressed up as a result.
                // The read itself would fail, but only after the allocation succeeded or the
                // process died trying.
                let start = ptr as u32 as usize;
                let count = len as u32 as usize;
                if start
                    .checked_add(count)
                    .is_none_or(|end| end > memory.data_size(&caller))
                {
                    return;
                }
                let mut buffer = vec![0u8; count];
                if memory.read(&caller, start, &mut buffer).is_ok() {
                    caller.data_mut().output = buffer;
                }
            },
        )
        .map_err(|e| format!("could not link output: {e}"))?;

    let instance = linker
        .instantiate(&mut store, &module)
        .map_err(|e| format!("could not instantiate: {e}"))?;
    let entry = instance
        .get_typed_func::<(), ()>(&mut store, validate::ENTRY_POINT)
        .map_err(|e| format!("module does not export {}: {e}", validate::ENTRY_POINT))?;

    entry
        .call(&mut store, ())
        .map_err(|e| format!("execution trapped: {e}"))?;

    // Two encodings, one number. `Metering::Global` counts into an exported global — free enough
    // to leave on — and `Metering::HostCall` counts through the import linked above. A module
    // metered neither way reports nothing, which is not a failure.
    let counted = instance
        .get_global(&mut store, validate::FUEL_EXPORT)
        .and_then(|global| global.get(&mut store).i64())
        .map(|value| value as u64);
    let charged = store.data().fuel;

    Ok(Executed {
        output: std::mem::take(&mut store.data_mut().output),
        fuel: counted.or((charged > 0).then_some(charged)),
    })
}
