// Runs a corpus of work units on the browser volunteer's own host, and says what happened.
//
// # Why this file exists
//
// The determinism gate in `runtime/tests/differential.rs` compares Cairn's interpreter against
// wasmi and wasmtime. Both are Rust, both are linked into the same test binary, and neither is
// the engine a volunteer actually uses. **The engine this project is *for* is the one in the
// browser**, and until this file existed it was the only engine in the system that nothing
// checked.
//
// That gap mattered more than its size suggests. Cairn settles a dispute by finding the first
// instruction at which two executions diverged; the scheme is sound only while two *honest*
// engines running the same bytes agree exactly. A browser volunteer that disagreed with Cairn's
// interpreter would not be caught cheating — it would be **convicted of cheating**, for running
// in a browser.
//
// # What it is not
//
// It is not a second implementation of the volunteer. It imports `./host.js` — the same module
// `worker.js` imports, the same three host functions, the same fuel reading — because a harness
// that reimplemented any of that would be testing itself.
//
// **It is also not told the expected answers.** The manifest carries a module and an input and
// nothing else; the comparison happens in Rust, against outcomes Rust produced. A harness that
// knew the answer is a harness that can launder one.
//
// # Usage
//
//     node browser/differential.js <directory>
//
// The directory holds `manifest.tsv` — one `index<TAB>inputHex` line per case, with the module
// in `case-<index>.wasm` — and receives `results.tsv`, one `index<TAB>ok|trap<TAB>fuel<TAB>hex`
// line per case. Tab-separated rather than JSON because both ends are a few lines of parsing
// either way, and this way neither end needs a parser at all.

import { readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';

import { runUnit } from './host.js';

function unhex(text) {
  const bytes = new Uint8Array(text.length / 2);
  for (let i = 0; i < bytes.length; i += 1) {
    bytes[i] = parseInt(text.substr(i * 2, 2), 16);
  }
  return bytes;
}

function hex(bytes) {
  let out = '';
  for (const byte of bytes) out += byte.toString(16).padStart(2, '0');
  return out;
}

const directory = process.argv[2];
if (!directory) {
  console.error('usage: node browser/differential.js <directory>');
  process.exit(2);
}

const manifest = readFileSync(join(directory, 'manifest.tsv'), 'utf8')
  .split('\n')
  .map((line) => line.trim())
  .filter((line) => line.length > 0)
  .map((line) => {
    const [index, input] = line.split('\t');
    return { index, input: unhex(input ?? '') };
  });

const results = [];

for (const unit of manifest) {
  const bytes = readFileSync(join(directory, `case-${unit.index}.wasm`));

  try {
    const { output, fuel } = await runUnit(bytes, unit.input);
    results.push(`${unit.index}\tok\t${fuel === null ? '0' : fuel.toString()}\t${hex(output)}`);
  } catch (trap) {
    // A trap is a result, not a failure of the harness — a workload that divides by zero has
    // failed deterministically, and every honest volunteer fails it in the same place. What
    // matters for the comparison is that it trapped and how far it got, and `host.js` attaches
    // the second because a thrown error is the only channel a trap has.
    const fuel = trap && typeof trap === 'object' && trap.cairnFuel != null ? trap.cairnFuel : 0n;
    results.push(`${unit.index}\ttrap\t${fuel.toString()}\t`);
  }
}

writeFileSync(join(directory, 'results.tsv'), `${results.join('\n')}\n`);
console.log(`${results.length} units run on ${process.report?.getReport()?.header?.componentVersions?.v8 ?? 'V8'}`);
