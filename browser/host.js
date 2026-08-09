// Cairn's host interface, in JavaScript, around the engine the browser already has.
//
// # Why there is no WebAssembly engine in this directory
//
// There is a perfectly good one in the page, and using it is the entire point of
// [ADR-0005](../docs/adr/0005-the-fast-path-cannot-snapshot.md). A volunteer executes the
// canonical module on V8 or SpiderMonkey at full speed and returns a result. It cannot produce
// a trace — no browser exposes the operand stack, and none ever will — so it does not try. If
// somebody disputes the result, a *different* path re-executes the same unit under Cairn's
// interpreter, and that path is Rust and lives in `runtime/`.
//
// So this file is glue, and being glue is the achievement. Three imported functions and a
// global to read afterwards is the whole contract between the network and a volunteer.
//
// # What it does not do
//
// It does not instrument. The bytes it runs are the canonical binary a coordinator produced at
// registration — `cairn-worker prepare` writes exactly those — and their hash is the unit's
// identity. A volunteer that could rewrite its own work unit would be a volunteer whose result
// means nothing.

/// A workload cannot be trusted with the length it passes to `output`.
///
/// This is the same hazard the Rust hosts guard, and it is worth guarding in the same order
/// for the same reason: bounds-check *before* allocating. `len` is a value the workload chose,
/// a generated one will happily ask for four gigabytes, and a copy that large is a denial of
/// service against the volunteer's own machine dressed up as a result.
function readMemory(memory, ptr, len) {
  const start = ptr >>> 0;
  const count = len >>> 0;
  if (start + count > memory.buffer.byteLength) {
    return null;
  }
  return new Uint8Array(memory.buffer, start, count).slice();
}

/**
 * Execute one work unit and report what it produced.
 *
 * @param {BufferSource} bytes  The canonical module, as `cairn-worker prepare` wrote it.
 * @param {Uint8Array}   input  The unit's input.
 * @returns {Promise<{output: Uint8Array, fuel: bigint|null, milliseconds: number}>}
 */
export async function runUnit(bytes, input) {
  let memory = null;
  let output = new Uint8Array(0);

  // Counted only when the module meters through the host call. The honest path does not meter
  // at all and the global encoding does not call this, so on the two configurations a
  // volunteer actually receives, this stays zero — but the import must exist regardless,
  // because the instrumentation pass appends it to *every* module it emits, whether or not
  // anything calls it. Leaving it out fails instantiation, which is a confusing way to
  // discover that the pass keeps function indices stable by always adding the import.
  let chargedFuel = 0n;

  const imports = {
    cairn: {
      // Copies up to `len` bytes to `ptr` and returns the input's true length, so a workload
      // can size its buffer with a zero-length probe. The example unit does exactly that.
      input: (ptr, len) => {
        const available = input.length;
        const count = Math.min(available, len >>> 0);
        if (count > 0 && memory) {
          const start = ptr >>> 0;
          if (start + count <= memory.buffer.byteLength) {
            new Uint8Array(memory.buffer).set(input.subarray(0, count), start);
          }
          // A failed write means the workload named an out-of-bounds address. It will trap on
          // its own shortly; the host does not need to judge.
        }
        return available;
      },

      output: (ptr, len) => {
        if (!memory) return;
        const bytes = readMemory(memory, ptr, len);
        if (bytes) output = bytes;
      },

      charge: (instructions) => {
        chargedFuel += BigInt(instructions >>> 0);
      },
    },
  };

  const { instance } = await WebAssembly.instantiate(bytes, imports);
  memory = instance.exports.memory;

  const started = performance.now();
  instance.exports.cairn_run();
  const milliseconds = performance.now() - started;

  return { output, fuel: readFuel(instance, chargedFuel), milliseconds };
}

/// How much work the unit was, if the unit was prepared to say.
///
/// The interesting half of [ADR-0009](../docs/adr/0009-metering-through-a-global-the-engines-disagree.md):
/// a module metered through an exported counter can be run by an engine Cairn does not control
/// and still report an exact, machine-independent instruction count. Two lines here; it was
/// unavailable at any acceptable price when metering meant a host call per basic block.
///
/// `null` means the unit was not prepared with `--count-fuel`, which is the default. There is
/// no way to recover the number after the fact, and no reason to want one — the volunteer did
/// the work either way.
function readFuel(instance, chargedFuel) {
  const counter = instance.exports.cairn_fuel;
  if (counter) return counter.value;
  return chargedFuel > 0n ? chargedFuel : null;
}
