//! The instruction counter that gives an execution trace its coordinate system.
//!
//! Dispute arbitration works by naming a point in an execution — "the state after
//! instruction 4,192,003" — and asking two workers what the machine looked like there. That
//! only means anything if **every machine counts instructions identically.** Fuel is that
//! shared coordinate.
//!
//! # Why counting is batched
//!
//! Incrementing a counter before every instruction would roughly double the cost of cheap
//! instructions. Instead the instrumentation pass charges each basic block once, on entry,
//! for the number of instructions it contains. A block's instruction count is fixed at
//! rewrite time and identical for every worker, so the running total is exact even though it
//! advances in jumps.
//!
//! # Fuel is a budget, not an address
//!
//! Because charging is batched, fuel advances in jumps: every instruction between two
//! `charge` calls shares one fuel value. **"The state at fuel F" is therefore ambiguous**, and
//! fuel cannot serve as the coordinate dispute bisection binary-searches over.
//!
//! An earlier version of this document claimed it could, on the reasoning that a worker can be
//! asked for the root at any fuel value. The replay part of that is right; the addressing part
//! is not, because many distinct states share a fuel value.
//!
//! The coordinate is the **step index** instead — see [`crate::engine::machine::Snapshot`].
//! [`crate::engine::machine::Machine::step`] executes exactly one instruction, so step *n*
//! names exactly one state. Fuel remains what it was built to be: the budget that bounds an
//! execution and the unit the network accounts costs in.
//!
//! Snapshot boundaries are still decided by fuel, since that is what the instrumentation pass
//! meters. They simply land at whichever step index the crossing happened to occur at, and are
//! labelled with both.
//!
//! # Determinism rules
//!
//! Three behaviours must be identical on every machine, and each has a test below:
//!
//! 1. **Exhaustion is detected at block granularity.** If a block would push the total past
//!    the limit, the trap is reported at the block's entry fuel value — never partway
//!    through. Engines never have to agree on how far into a block they got.
//! 2. **A block spanning several snapshot thresholds produces one snapshot, not several.**
//!    The schedule advances to the next threshold strictly above the new total.
//! 3. **Arithmetic overflow is exhaustion**, not a wrap and not a panic.

use std::fmt;

/// A position in an execution, measured in instructions retired since entry.
///
/// This is the coordinate every part of the dispute protocol speaks in: snapshots are
/// labelled with it, bisection narrows a range of it, and arbitration names a single value of
/// it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct Fuel(u64);

impl Fuel {
    /// The start of any execution.
    pub const ZERO: Self = Self(0);

    /// Construct a fuel value from a raw instruction count.
    #[must_use]
    pub const fn new(instructions: u64) -> Self {
        Self(instructions)
    }

    /// The raw instruction count.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for Fuel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The largest permitted snapshot interval exponent.
///
/// `2^63` instructions is far beyond any executable workload; the bound exists so that
/// interval arithmetic cannot overflow rather than to express a real policy.
pub const MAX_INTERVAL_LOG2: u8 = 63;

/// Rejected [`FuelMeter`] configurations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigError {
    /// The snapshot interval exponent exceeded [`MAX_INTERVAL_LOG2`].
    IntervalTooLarge {
        /// The exponent that was requested.
        requested: u8,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IntervalTooLarge { requested } => write!(
                f,
                "snapshot interval 2^{requested} exceeds the maximum 2^{MAX_INTERVAL_LOG2}"
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

/// What the caller must do after charging a block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "an outcome of Exhausted or SnapshotDue changes what the engine must do next"]
pub enum Charge {
    /// The block was charged. Continue executing.
    Continue,

    /// The block was charged, and execution has crossed a snapshot threshold. Commit the
    /// current machine state before executing the block, labelled with the enclosed fuel
    /// value.
    SnapshotDue {
        /// The fuel value this snapshot is labelled with: the total *before* this block ran.
        at: Fuel,
    },

    /// The limit would be exceeded. Execution must trap here without running the block.
    ///
    /// The enclosed value is the fuel at the point of the trap — the block's entry, since
    /// exhaustion is always detected at block granularity.
    Exhausted {
        /// The fuel value at which execution traps.
        at: Fuel,
    },
}

/// Tracks instructions retired, enforces a ceiling, and schedules trace snapshots.
///
/// A meter is created per work unit execution, from the limits declared in the unit's
/// manifest. Both the fast path and the instrumented interpreter drive an identical meter,
/// which is what makes their traces comparable.
///
/// # Examples
///
/// ```
/// use cairn_runtime::fuel::{Charge, Fuel, FuelMeter};
///
/// // Snapshot every 2^3 = 8 instructions, with a 1000-instruction ceiling.
/// let mut meter = FuelMeter::new(1_000, 3).unwrap();
///
/// assert_eq!(meter.charge(5), Charge::Continue);
/// assert_eq!(meter.consumed(), Fuel::new(5));
///
/// // Crossing the 8-instruction threshold schedules a snapshot, labelled with the fuel
/// // value *before* the block ran.
/// assert_eq!(meter.charge(5), Charge::SnapshotDue { at: Fuel::new(5) });
/// assert_eq!(meter.consumed(), Fuel::new(10));
/// ```
#[derive(Debug, Clone)]
pub struct FuelMeter {
    consumed: u64,
    limit: u64,
    interval_log2: u8,
    /// The next total at or above which a snapshot becomes due.
    next_threshold: u64,
}

impl FuelMeter {
    /// Create a meter with an instruction ceiling and a snapshot interval of `2^interval_log2`.
    ///
    /// An `interval_log2` of 0 snapshots every instruction, which is only useful in tests.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::IntervalTooLarge`] if `interval_log2` exceeds
    /// [`MAX_INTERVAL_LOG2`].
    pub fn new(limit: u64, interval_log2: u8) -> Result<Self, ConfigError> {
        if interval_log2 > MAX_INTERVAL_LOG2 {
            return Err(ConfigError::IntervalTooLarge {
                requested: interval_log2,
            });
        }
        Ok(Self {
            consumed: 0,
            limit,
            interval_log2,
            // The state at fuel 0 is committed separately as the execution's initial root,
            // so the first scheduled snapshot is one interval in.
            next_threshold: 1u64 << interval_log2,
        })
    }

    /// Resume a meter partway through an execution.
    ///
    /// Used by dispute adjudication, which rebuilds a machine at the disputed instruction
    /// rather than running up to it. The snapshot schedule is advanced to wherever it would
    /// have been at that point, so a resumed meter behaves exactly like one that got there the
    /// long way.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::IntervalTooLarge`] on the same terms as [`FuelMeter::new`].
    pub fn restore(consumed: Fuel, limit: u64, interval_log2: u8) -> Result<Self, ConfigError> {
        let mut meter = Self::new(limit, interval_log2)?;
        meter.consumed = consumed.0;
        meter.advance_threshold_past(consumed.0);
        Ok(meter)
    }

    /// Instructions retired so far.
    #[must_use]
    pub const fn consumed(&self) -> Fuel {
        Fuel(self.consumed)
    }

    /// The instruction ceiling this execution was created with.
    #[must_use]
    pub const fn limit(&self) -> u64 {
        self.limit
    }

    /// Instructions remaining before exhaustion.
    #[must_use]
    pub const fn remaining(&self) -> u64 {
        self.limit.saturating_sub(self.consumed)
    }

    /// Charge for a basic block of `instructions` instructions, about to be executed.
    ///
    /// Call this on entry to every block, before running it. The returned [`Charge`] says
    /// whether to proceed, to commit a snapshot first, or to trap.
    ///
    /// When the result is [`Charge::Exhausted`] the meter is left untouched, so the reported
    /// fuel value is the block's entry point and repeated calls report the same thing.
    pub fn charge(&mut self, instructions: u64) -> Charge {
        let entry = Fuel(self.consumed);

        let Some(total) = self.consumed.checked_add(instructions) else {
            // Overflow is treated as exhaustion so that behaviour past 2^64 instructions is
            // defined rather than wrapping. No real workload reaches this.
            return Charge::Exhausted { at: entry };
        };

        if total > self.limit {
            return Charge::Exhausted { at: entry };
        }

        self.consumed = total;

        if total >= self.next_threshold {
            self.advance_threshold_past(total);
            return Charge::SnapshotDue { at: entry };
        }

        Charge::Continue
    }

    /// Move the schedule to the first threshold strictly greater than `total`.
    ///
    /// A block large enough to span several thresholds yields exactly one snapshot. Skipping
    /// is deliberate: taking several snapshots of the same machine state would commit the
    /// same root repeatedly and tell arbitration nothing new.
    fn advance_threshold_past(&mut self, total: u64) {
        let interval = 1u64 << self.interval_log2;

        // The next multiple of `interval` strictly above `total`.
        let steps = total / interval + 1;
        // Past the last representable threshold no further snapshot can be due. Saturating
        // here is unreachable in practice because `charge` treats overflow as exhaustion
        // first, but it keeps the arithmetic total rather than relying on that.
        self.next_threshold = steps.saturating_mul(interval);
    }

    /// The fuel value at which the next snapshot becomes due.
    ///
    /// Exposed for tests and for the dispute protocol, which needs to know which fuel values
    /// a worker pre-committed to before asking it to derive an intermediate one.
    #[must_use]
    pub const fn next_threshold(&self) -> Fuel {
        Fuel(self.next_threshold)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn meter(limit: u64, interval_log2: u8) -> FuelMeter {
        FuelMeter::new(limit, interval_log2).unwrap()
    }

    #[test]
    fn charging_accumulates() {
        let mut m = meter(1_000, 10);
        assert_eq!(m.charge(3), Charge::Continue);
        assert_eq!(m.charge(4), Charge::Continue);
        assert_eq!(m.consumed(), Fuel::new(7));
        assert_eq!(m.remaining(), 993);
    }

    #[test]
    fn snapshot_is_labelled_with_the_fuel_before_the_block() {
        // The snapshot commits the state as it was on entry to the block that crossed the
        // threshold. Labelling it with the post-block total would make the label describe a
        // state that was never committed.
        let mut m = meter(1_000, 3); // threshold at 8
        assert_eq!(m.charge(6), Charge::Continue);
        assert_eq!(m.charge(4), Charge::SnapshotDue { at: Fuel::new(6) });
        assert_eq!(m.consumed(), Fuel::new(10));
    }

    #[test]
    fn threshold_advances_past_the_new_total() {
        let mut m = meter(1_000, 3); // thresholds at 8, 16, 24, ...
        assert_eq!(m.next_threshold(), Fuel::new(8));

        let _ = m.charge(9); // total 9, crosses 8
        assert_eq!(m.next_threshold(), Fuel::new(16));

        let _ = m.charge(1); // total 10, no crossing
        assert_eq!(m.next_threshold(), Fuel::new(16));
    }

    #[test]
    fn landing_exactly_on_a_threshold_snapshots() {
        let mut m = meter(1_000, 3);
        assert_eq!(m.charge(8), Charge::SnapshotDue { at: Fuel::ZERO });
        assert_eq!(m.next_threshold(), Fuel::new(16));
    }

    #[test]
    fn a_block_spanning_many_thresholds_snapshots_once() {
        // Determinism rule 2. A 100-instruction block crosses thresholds 8, 16, ..., 96 but
        // there is only one machine state to commit, so there is one snapshot.
        let mut m = meter(1_000, 3);
        assert_eq!(m.charge(100), Charge::SnapshotDue { at: Fuel::ZERO });
        assert_eq!(m.consumed(), Fuel::new(100));
        assert_eq!(m.next_threshold(), Fuel::new(104));
        assert_eq!(m.charge(1), Charge::Continue);
    }

    #[test]
    fn exhaustion_is_reported_at_block_entry() {
        // Determinism rule 1. Two engines must never have to agree on how far into an
        // over-budget block they managed to get.
        let mut m = meter(10, 10);
        assert_eq!(m.charge(7), Charge::Continue);
        assert_eq!(m.charge(9), Charge::Exhausted { at: Fuel::new(7) });
        assert_eq!(
            m.consumed(),
            Fuel::new(7),
            "an exhausted charge must not consume"
        );
    }

    #[test]
    fn exhaustion_is_stable_under_repetition() {
        let mut m = meter(10, 10);
        let _ = m.charge(10);
        assert_eq!(m.charge(1), Charge::Exhausted { at: Fuel::new(10) });
        assert_eq!(m.charge(1), Charge::Exhausted { at: Fuel::new(10) });
        assert_eq!(m.charge(500), Charge::Exhausted { at: Fuel::new(10) });
    }

    #[test]
    fn spending_exactly_the_limit_is_allowed() {
        let mut m = meter(10, 10);
        assert_eq!(m.charge(10), Charge::Continue);
        assert_eq!(m.remaining(), 0);
        assert_eq!(m.charge(1), Charge::Exhausted { at: Fuel::new(10) });
    }

    #[test]
    fn overflow_is_exhaustion_not_a_wrap() {
        // Determinism rule 3.
        let mut m = meter(u64::MAX, 10);
        assert_eq!(
            m.charge(u64::MAX - 1),
            Charge::SnapshotDue { at: Fuel::ZERO }
        );
        assert_eq!(
            m.charge(u64::MAX),
            Charge::Exhausted {
                at: Fuel::new(u64::MAX - 1)
            }
        );
        assert_eq!(m.consumed(), Fuel::new(u64::MAX - 1));
    }

    #[test]
    fn zero_sized_blocks_do_not_advance_or_snapshot() {
        let mut m = meter(1_000, 3);
        assert_eq!(m.charge(0), Charge::Continue);
        assert_eq!(m.consumed(), Fuel::ZERO);
        assert_eq!(m.next_threshold(), Fuel::new(8));
    }

    #[test]
    fn interval_of_zero_snapshots_every_block() {
        let mut m = meter(100, 0);
        assert_eq!(m.charge(1), Charge::SnapshotDue { at: Fuel::ZERO });
        assert_eq!(m.charge(1), Charge::SnapshotDue { at: Fuel::new(1) });
        assert_eq!(m.charge(1), Charge::SnapshotDue { at: Fuel::new(2) });
    }

    #[test]
    fn rejects_an_oversized_interval() {
        // FuelMeter deliberately does not implement PartialEq — comparing two meters is
        // never a meaningful operation — so the error is unwrapped rather than compared as
        // a whole Result.
        assert_eq!(
            FuelMeter::new(100, MAX_INTERVAL_LOG2 + 1).unwrap_err(),
            ConfigError::IntervalTooLarge {
                requested: MAX_INTERVAL_LOG2 + 1
            }
        );
        assert!(FuelMeter::new(100, MAX_INTERVAL_LOG2).is_ok());
    }

    #[test]
    fn block_partitioning_does_not_change_the_trace() {
        // The single most important property in this module. The instrumentation pass decides
        // where block boundaries fall; if splitting a run of instructions into different
        // blocks changed the fuel totals, two engines could disagree for reasons that have
        // nothing to do with the computation.
        //
        // Snapshot *labels* legitimately depend on block boundaries — a snapshot commits the
        // state at a boundary — but the totals must not.
        let mut coarse = meter(10_000, 4);
        let _ = coarse.charge(64);

        let mut fine = meter(10_000, 4);
        for _ in 0..64 {
            let _ = fine.charge(1);
        }

        assert_eq!(coarse.consumed(), fine.consumed());
        assert_eq!(coarse.next_threshold(), fine.next_threshold());
    }

    #[test]
    fn snapshot_labels_are_monotonic_and_distinct() {
        // Bisection assumes committed fuel values are strictly increasing; a repeated or
        // out-of-order label would break the binary search invariant.
        let mut m = meter(10_000, 4);
        let mut labels = Vec::new();

        for block in [7u64, 13, 1, 40, 3, 9, 100, 2] {
            if let Charge::SnapshotDue { at } = m.charge(block) {
                labels.push(at.get());
            }
        }

        assert!(
            labels.len() >= 2,
            "expected several snapshots, got {labels:?}"
        );
        assert!(
            labels.is_sorted_by(|a, b| a < b),
            "labels must strictly increase: {labels:?}"
        );
    }
}
