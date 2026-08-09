// The Web Worker a volunteer's browser actually runs work in.
//
// It is small because it should be. A worker receives a unit, executes it, and posts back what
// came out — and the reason it is a worker at all is not speed but *containment*: a work unit
// is a program somebody else wrote, and it runs on a thread where it cannot touch the page.
//
// # The thing this file cannot do, and why that is written down rather than worked around
//
// **A running WebAssembly call cannot be interrupted from JavaScript.** There is no timeout,
// no cancellation token, no way to ask politely. Once `cairn_run` is entered, this thread
// belongs to the workload until it returns. The only lever is `worker.terminate()` from the
// page, which kills the thread outright and loses whatever was in flight.
//
// That is not a limitation to apologise for; it is the honest path's design, stated in
// [ADR-0009](../docs/adr/0009-metering-through-a-global-the-engines-disagree.md): enforcement
// on the honest path is *allowed* to be imprecise, because a volunteer who stops early has
// produced no answer rather than a wrong one. The unit is reassigned and nothing about
// verification is weakened. The precise, deterministic instruction ceiling exists on the
// dispute path, where a trap is a result two parties have to agree on.
//
// So the timeout lives in `index.html`, where it can terminate a worker, and this file just
// says what happened.

import { runUnit } from './host.js';

self.onmessage = async (event) => {
  const { id, module, input } = event.data;

  try {
    const { output, fuel, milliseconds } = await runUnit(module, new Uint8Array(input));
    self.postMessage({
      id,
      ok: true,
      output,
      // `postMessage` cannot clone a BigInt inside a plain object in every engine, and the
      // number can exceed 2^53 on a long unit, so it crosses as a decimal string. The page
      // formats it; nothing arithmetic is done with it on the far side.
      fuel: fuel === null ? null : fuel.toString(),
      milliseconds,
    });
  } catch (error) {
    // A trap is a legitimate outcome of a work unit, not a bug in the worker: a workload that
    // divides by zero or reads out of bounds has failed deterministically, and every honest
    // volunteer fails it in exactly the same place. The page reports it as a result.
    self.postMessage({ id, ok: false, error: String(error && error.message ? error.message : error) });
  }
};
