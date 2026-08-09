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

/// Compile and run `module` on wasmtime, returning what it wrote through `cairn.output`.
///
/// # Errors
///
/// Any failure to compile, link, instantiate or execute, as a message fit to print.
pub fn execute(module: &[u8], input: &[u8]) -> Result<Vec<u8>, String> {
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
                let mut buffer = vec![0u8; len as u32 as usize];
                if let Some(memory) = caller.get_export("memory").and_then(Extern::into_memory) {
                    if memory
                        .read(&caller, ptr as u32 as usize, &mut buffer)
                        .is_ok()
                    {
                        caller.data_mut().output = buffer;
                    }
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

    Ok(std::mem::take(&mut store.data_mut().output))
}
