// `node --test browser/`
//
// Only `policy.js` is tested here, and that is not a gap — it is why `policy.js` exists as a
// separate file. The rest of this directory is glue around browser APIs that node does not
// have; the decisions are all in one pure function, and a volunteer's manners towards the
// machine it is running on are exactly the kind of thing that should be stated as assertions
// rather than trusted to a comment.

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { decide, BATTERY_FLOOR, DEFAULT_SHARE } from './policy.js';

test('a plugged-in desktop takes work and leaves a core', () => {
  const verdict = decide({ hardwareConcurrency: 8, share: DEFAULT_SHARE });
  assert.equal(verdict.accept, true);
  assert.equal(verdict.workers, 4);
});

test('a single-core machine still contributes', () => {
  // Leaving a core is impossible here, and refusing would exclude exactly the machines whose
  // spare cycles are most worth having. The browser time-slices instead.
  const verdict = decide({ hardwareConcurrency: 1 });
  assert.equal(verdict.accept, true);
  assert.equal(verdict.workers, 1);
});

test('a full share never takes the last core', () => {
  const verdict = decide({ hardwareConcurrency: 4, share: 1 });
  assert.equal(verdict.workers, 3, 'the page still has to respond to the person who opened it');
});

test('data-saving mode outranks everything, including being plugged in', () => {
  // Save-Data is a request, not a capability hint. A volunteer who asked their browser to use
  // less data and then finds work units being fetched has been ignored.
  const verdict = decide({
    saveData: true,
    charging: true,
    batteryLevel: 1,
    hardwareConcurrency: 16,
  });
  assert.equal(verdict.accept, false);
  assert.equal(verdict.workers, 0);
  assert.match(verdict.reason, /data-saving/);
});

test('a 2g connection refuses work', () => {
  for (const effectiveType of ['2g', 'slow-2g']) {
    const verdict = decide({ effectiveType, hardwareConcurrency: 8 });
    assert.equal(verdict.accept, false, effectiveType);
  }
  assert.equal(decide({ effectiveType: '3g', hardwareConcurrency: 8 }).accept, true);
});

test('a low battery stops work rather than slowing it', () => {
  const verdict = decide({
    charging: false,
    batteryLevel: BATTERY_FLOOR - 0.01,
    hardwareConcurrency: 8,
  });
  assert.equal(verdict.accept, false);
  assert.match(verdict.reason, /battery/);
});

test('a laptop on battery but above the floor works at half width', () => {
  // The case worth getting right. An unplugged laptop at 80% is a perfectly good volunteer,
  // and a policy that refused it would refuse most of the machines people actually use.
  const plugged = decide({ charging: true, hardwareConcurrency: 8 });
  const unplugged = decide({ charging: false, batteryLevel: 0.8, hardwareConcurrency: 8 });

  assert.equal(unplugged.accept, true);
  assert.equal(unplugged.workers, Math.floor(plugged.workers / 2));
  assert.match(unplugged.reason, /half width/);
});

test('being charged at any level is enough', () => {
  const verdict = decide({ charging: true, batteryLevel: 0.01, hardwareConcurrency: 4 });
  assert.equal(verdict.accept, true);
});

test('a backgrounded tab keeps one worker rather than none', () => {
  // Not generosity: a unit already in flight finishes instead of being abandoned half-done,
  // and the browser throttles the thread anyway.
  const verdict = decide({ hidden: true, hardwareConcurrency: 16 });
  assert.equal(verdict.accept, true);
  assert.equal(verdict.workers, 1);
  assert.match(verdict.reason, /background/);
});

test('missing information is read permissively', () => {
  // Firefox and Safari do not expose the Battery Status API at all. If an absent field meant
  // "refuse", the policy would exclude most of the web without ever saying so.
  const verdict = decide({});
  assert.equal(verdict.accept, true);
  assert.ok(verdict.workers >= 1);
});

test('a share of zero still leaves one worker', () => {
  // Zero workers and `accept: true` would be a state the caller has to special-case, and the
  // way to decline work is `accept: false`. There is exactly one way to say no.
  const verdict = decide({ share: 0, hardwareConcurrency: 8 });
  assert.equal(verdict.accept, true);
  assert.equal(verdict.workers, 1);
});

test('an out-of-range share is clamped rather than trusted', () => {
  assert.equal(decide({ share: 99, hardwareConcurrency: 4 }).workers, 3);
  assert.equal(decide({ share: -5, hardwareConcurrency: 4 }).workers, 1);
});
