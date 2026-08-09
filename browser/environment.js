// Everything that touches `navigator`, kept apart from everything that decides.
//
// The split is the point: `policy.js` is pure and tested, this file is impure and cannot be.
// It is small on purpose, and it does one thing that is easy to get wrong — it treats every
// missing API as the *permissive* value and says so, because Firefox and Safari do not expose
// the Battery Status API at all and a volunteer on those browsers must not be refused work by
// an absence.

/**
 * Read the current environment, for `decide` in `policy.js`.
 *
 * @param {number} share Share of cores to use, 0..1.
 * @returns {Promise<object>}
 */
export async function readEnvironment(share) {
  const connection = navigator.connection || {};

  // Battery Status is Chromium-only, and behind a permission on some builds. Absent means
  // "assume plugged in and full", which errs towards taking work — the alternative errs
  // towards refusing every Firefox and Safari volunteer on the network.
  let charging = true;
  let batteryLevel = 1;
  let batterySource = 'unavailable — assuming plugged in';

  if (typeof navigator.getBattery === 'function') {
    try {
      const battery = await navigator.getBattery();
      charging = battery.charging;
      batteryLevel = battery.level;
      batterySource = 'Battery Status API';
    } catch {
      // A rejected promise means the browser has the API and declined to answer. Same
      // treatment as not having it: this is not a reason to refuse a volunteer.
    }
  }

  return {
    charging,
    batteryLevel,
    batterySource,
    saveData: Boolean(connection.saveData),
    effectiveType: connection.effectiveType || '4g',
    hardwareConcurrency: navigator.hardwareConcurrency || 1,
    hidden: document.hidden,
    share,
  };
}
