// Talking to a coordinator: take a unit, do it, report the answer, repeat.
//
// This is the file that turns the page from a demonstration into a volunteer. Everything else
// in this directory works on a unit somebody handed it; this asks for one.
//
// # The whole protocol
//
//     GET  /api/lease?worker=NAME              → 200 {unit, workload, input}  or 204
//     GET  /api/module/{workload}              → the canonical bytes
//     POST /api/result?unit=N&worker=NAME&fuel=F, body = the answer in hex
//
// Four calls and no state on the client beyond a name. That is deliberate: a volunteer is
// something that can close the tab mid-unit and cost the network one reassignment.
//
// # What a volunteer is *not* asked to do
//
// It is never asked to prove anything. It runs the unit and returns eight bytes. No trace, no
// commitment, no signature — and no way to produce one, which is the finding
// [ADR-0005](../docs/adr/0005-the-fast-path-cannot-snapshot.md) is about. Every scheme that
// would make a volunteer's answer self-verifying was ruled out before this file was written,
// which is why this file is short.

import { runUnit } from './host.js';

/// Modules are fetched once and kept: a coordinator hands out many units of the same workload,
/// and re-downloading the module for each one would be the only part of this that scales badly.
const modules = new Map();

/**
 * Take one unit and do it. Returns what happened, for the page to display.
 *
 * @param {string} base    coordinator origin, e.g. '' for same-origin
 * @param {string} worker  this volunteer's name
 * @returns {Promise<{idle: true} | {idle: false, unit: number, fuel: bigint|null, milliseconds: number, outcome: object}>}
 */
export async function doOneUnit(base, worker) {
  const lease = await fetch(`${base}/api/lease?worker=${encodeURIComponent(worker)}`);

  // 204 is "nothing to do", which is neither an error nor a result. A volunteer that treated it
  // as either would spin or stop; it should wait and ask again.
  if (lease.status === 204) return { idle: true };
  if (!lease.ok) throw new Error(`lease: ${lease.status}`);

  const { unit, workload, input } = await lease.json();

  let module = modules.get(workload);
  if (!module) {
    const response = await fetch(`${base}/api/module/${workload}`);
    if (!response.ok) throw new Error(`module ${workload}: ${response.status}`);
    module = new Uint8Array(await response.arrayBuffer());
    modules.set(workload, module);
  }

  const { output, fuel, milliseconds } = await runUnit(module, unhex(input));

  // The fuel figure is advisory and the coordinator says so: a volunteer could report anything.
  // It is here because it is the thing ADR-0009 made affordable — a network that can account
  // for contributed work rather than only count completed units.
  const query = new URLSearchParams({ unit: String(unit), worker });
  if (fuel !== null) query.set('fuel', fuel.toString());

  const submitted = await fetch(`${base}/api/result?${query}`, {
    method: 'POST',
    body: hex(output),
  });
  if (!submitted.ok) throw new Error(`result: ${submitted.status} ${await submitted.text()}`);

  return { idle: false, unit, fuel, milliseconds, outcome: await submitted.json() };
}

function hex(bytes) {
  return [...bytes].map((b) => b.toString(16).padStart(2, '0')).join('');
}

function unhex(text) {
  const out = new Uint8Array(text.length / 2);
  for (let i = 0; i < out.length; i += 1) {
    out[i] = Number.parseInt(text.substr(i * 2, 2), 16);
  }
  return out;
}
