// When a browser volunteer should take work, and how much of the machine to use.
//
// This is the only file here with decisions in it, so it is deliberately pure: it takes a
// plain description of the environment and returns a plain verdict. No `navigator`, no DOM, no
// timers. Everything that has to be true about a volunteer's manners can therefore be stated
// as a test rather than as a comment, which matters more here than it looks — a background
// worker that drains a phone or spends someone's mobile data is not a bug the user reports,
// it is a tab they close forever.
//
// The environment description is gathered in `environment.js`, which is the part that touches
// `navigator` and is not testable in node.

/// Share of the machine a volunteer takes when nothing is holding it back.
///
/// Not 1.0, and not configurable to 1.0 here: the page the worker lives in still has to
/// respond to the person who opened it, and a browser that stutters is a browser that gets
/// closed. Leaving a core is cheaper than losing the volunteer.
export const DEFAULT_SHARE = 0.5;

/// Below this, on battery, we stop entirely rather than slow down.
export const BATTERY_FLOOR = 0.2;

/// Connection types where fetching a work unit costs more than the work is worth.
const SLOW_CONNECTIONS = new Set(['slow-2g', '2g']);

/**
 * Decide whether to take work, and with how many workers.
 *
 * @param {object} env
 * @param {boolean} [env.charging]            Plugged in. Unknown is treated as plugged in.
 * @param {number}  [env.batteryLevel]        0..1. Unknown is treated as full.
 * @param {boolean} [env.saveData]            The user asked their browser to save data.
 * @param {string}  [env.effectiveType]       '4g', '3g', '2g', 'slow-2g'.
 * @param {number}  [env.hardwareConcurrency] Logical cores. Unknown is treated as 1.
 * @param {number}  [env.share]               Share of cores to use, 0..1.
 * @param {boolean} [env.hidden]              The tab is in the background.
 * @returns {{accept: boolean, workers: number, reason: string}}
 */
export function decide(env = {}) {
  const {
    charging = true,
    batteryLevel = 1,
    saveData = false,
    effectiveType = '4g',
    hardwareConcurrency = 1,
    share = DEFAULT_SHARE,
    hidden = false,
  } = env;

  // Save-Data is an explicit request, not a hint about capability, so it outranks everything
  // else including being plugged in. A volunteer who has said "use less of my data" and then
  // finds a page downloading work units has been ignored.
  if (saveData) {
    return { accept: false, workers: 0, reason: 'the browser is in data-saving mode' };
  }

  if (SLOW_CONNECTIONS.has(effectiveType)) {
    return {
      accept: false,
      workers: 0,
      reason: `the connection is ${effectiveType}, so fetching a unit costs more than the unit is worth`,
    };
  }

  if (!charging && batteryLevel < BATTERY_FLOOR) {
    return {
      accept: false,
      workers: 0,
      reason: `on battery at ${Math.round(batteryLevel * 100)}%, below the ${Math.round(
        BATTERY_FLOOR * 100,
      )}% floor`,
    };
  }

  // Everything from here is a matter of *how much*, not *whether*.
  const cores = Math.max(1, Math.floor(hardwareConcurrency));
  let workers = Math.round(cores * clamp(share, 0, 1));

  // Leave the machine a core, so the page the worker lives in stays usable. On a single-core
  // machine there is no core to leave and the browser time-slices instead.
  workers = Math.min(workers, Math.max(1, cores - 1));

  const notes = [];

  // On battery but above the floor: still useful, but not at full width. Halving rather than
  // stopping is the point — a laptop at 80% unplugged is a perfectly good volunteer.
  if (!charging) {
    workers = Math.max(1, Math.floor(workers / 2));
    notes.push('on battery, so at half width');
  }

  // A background tab is throttled by the browser anyway. Taking one worker rather than none
  // means a unit already in flight finishes instead of being abandoned half-done.
  if (hidden) {
    workers = 1;
    notes.push('tab is in the background');
  }

  workers = Math.max(1, workers);

  return {
    accept: true,
    workers,
    reason: notes.length
      ? `${workers} of ${cores} cores — ${notes.join('; ')}`
      : `${workers} of ${cores} cores`,
  };
}

function clamp(value, low, high) {
  return Math.min(high, Math.max(low, value));
}
