//! How much of a donated machine to use — decided by the machine, never asked of the operator.
//!
//! # Why this is not a `--jobs` flag
//!
//! A volunteer runs programs somebody else wrote, on a computer somebody else owns and is
//! probably also using. The number of those programs to run at once is the one setting where a
//! plausible answer is dangerous: a workload may declare up to
//! `validate::Limits::max_memory_pages` pages, which is 256 MiB, and a person who types `32`
//! because they have 32 threads has asked for 8 GiB of a laptop they are still working on. The
//! machine would swap, the donation would stop being free, and the volunteer would leave.
//!
//! So the count is derived, and the operator's flags can only ever make it *smaller*.
//!
//! # What actually bounds it
//!
//! Not cores. Work units share nothing — each gets its own instance, its own linear memory, and
//! runs single-threaded with no locks — so logically a machine could run one per core. **Memory
//! is what runs out first**, and it does so unevenly, because each workload declares its own
//! ceiling and a grid may serve several.
//!
//! That is why concurrency here is enforced by an [`Allowance`] of *bytes* rather than by a count
//! of threads. A thread claims what its unit declared before executing and gives it back after,
//! so the invariant is the one worth having:
//!
//! > the declared memory of all units executing at once never exceeds the budget.
//!
//! It holds without re-planning when a bigger workload shows up, and it holds for a grid mixing a
//! 1-page workload with a 4096-page one, which no fixed job count does.
//!
//! # The part that is easy to forget
//!
//! Executing a unit is not the largest thing a volunteer does. **Arguing about one is.** A party
//! to a dispute keeps up to [`DEFAULT_CHECKPOINT_BUDGET`] resume points so that
//! answering `log₂(n)` questions costs `O(n)` instead of `O(n log n)`, and each of those is a
//! full clone of the machine. A volunteer that budgeted only for execution would fit exactly as
//! many units as it had memory for and then be asked to defend one of them — and the worst
//! outcome this project has is an honest volunteer that loses an argument.
//!
//! So the budget is split rather than spent: **half** for running units, and a **quarter** the
//! dispute path may draw on, sized by [`checkpoints`]. The remaining quarter is the machine's,
//! which is the whole point of donating rather than surrendering.

use std::sync::{Arc, Condvar, Mutex};

use cairn_runtime::dispute::DEFAULT_CHECKPOINT_BUDGET;

/// The only WebAssembly page size Cairn admits, so the only one a page count converts through.
const PAGE: u64 = 64 * 1024;

/// What to assume about a machine whose free memory cannot be read.
///
/// Deliberately small. Guessing high costs somebody else's machine; guessing low costs this
/// project some throughput on a platform where `--memory` was not passed, and prints a line
/// saying exactly that. Only one of those is recoverable by reading the output.
const ASSUMED_MEMORY: u64 = 2 * 1024 * 1024 * 1024;

/// Never execute more than this many units at once, whatever the machine could hold.
///
/// The bound is **blast radius**, not capacity. A volunteer holding *n* leases that loses power
/// costs the grid *n* reassignments and delays *n* units by a lease timeout, and beyond a few
/// dozen the network is better served by that machine running two volunteer processes — two
/// names, two failure domains, and each one still one vote per unit.
pub const MOST_UNITS_AT_ONCE: usize = 32;

/// What was learned about the machine before deciding anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Survey {
    /// Hardware threads, as the operating system reports them.
    ///
    /// Note that this counts SMT siblings, which are not whole cores. Expect around 10–30% from
    /// the second thread of a core on work like this, not 100%: a WebAssembly interpreter loop is
    /// compute- and branch-bound, which is what SMT helps least.
    pub cores: usize,
    /// Memory this volunteer may plan around.
    pub memory: Budget,
}

/// A memory budget, carrying whether anyone actually knows it.
///
/// Kept as a distinction rather than collapsed to a number, because the printed header has to be
/// able to say which one it was. A volunteer reporting "2 GiB" when it in fact read nothing and
/// assumed is how an operator ends up trusting a guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Budget {
    /// Stated by the operator, who is the authority on their own machine.
    Stated(u64),
    /// Read from the operating system.
    Measured(u64),
    /// Nobody knows. [`ASSUMED_MEMORY`], and the header says so.
    Assumed(u64),
}

impl Budget {
    /// The number, whatever its provenance.
    #[must_use]
    pub const fn bytes(self) -> u64 {
        match self {
            Self::Stated(bytes) | Self::Measured(bytes) | Self::Assumed(bytes) => bytes,
        }
    }

    /// How to describe it in one line of output.
    #[must_use]
    pub const fn provenance(self) -> &'static str {
        match self {
            Self::Stated(_) => "stated with --memory",
            Self::Measured(_) => "read from the operating system",
            Self::Assumed(_) => "ASSUMED — this platform cannot be asked; pass --memory MiB",
        }
    }
}

/// Look at the machine. `stated` is the operator's `--memory`, in bytes, if they passed one.
///
/// # Why the measurement is Linux-only
///
/// `/proc/meminfo` is a file, and reading a file is something the standard library does. Every
/// other platform's answer is behind a C call — `GlobalMemoryStatusEx`, `host_statistics64` —
/// and this workspace denies `unsafe_code` at the root, for determinism reasons that have
/// nothing to do with convenience and are not worth weakening for a throughput heuristic. A
/// dependency would buy the same number at the cost of the rule that a dependency must do
/// something the standard library cannot, and `--memory` does it for free.
#[must_use]
pub fn survey(stated: Option<u64>) -> Survey {
    let cores = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    let memory = match stated {
        Some(bytes) => Budget::Stated(bytes),
        None => available_memory().map_or(Budget::Assumed(ASSUMED_MEMORY), Budget::Measured),
    };
    Survey { cores, memory }
}

/// Free memory as the operating system reports it, where it can be asked without `unsafe`.
fn available_memory() -> Option<u64> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    // `MemAvailable` and not `MemFree`: the kernel's own estimate of what a new workload could
    // get without swapping, which is the question being asked. `MemFree` excludes reclaimable
    // page cache and would under-report by most of a busy machine's RAM.
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kibibytes: u64 = meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemAvailable:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    kibibytes.checked_mul(1024)
}

/// How many units this machine is willing to have in flight, before memory is considered.
///
/// `ceiling` is the operator's `--jobs`. It can only lower the number: see the module docs for
/// why raising it is not offered.
///
/// One core is left alone. A volunteer that makes its owner's machine unpleasant to use is a
/// volunteer that gets uninstalled, and the last core is cheap insurance against that — on a
/// 16-thread machine it costs about 6% of the donation.
#[must_use]
pub fn threads(survey: &Survey, ceiling: Option<usize>) -> usize {
    let wanted = survey
        .cores
        .saturating_sub(1)
        .min(ceiling.unwrap_or(usize::MAX));
    // Never none: a single-core machine donates one thread. It is slower than the machine's owner
    // would like and still worth more than nothing. Never more than the blast-radius cap, however
    // large the machine or the flag.
    wanted.clamp(1, MOST_UNITS_AT_ONCE)
}

/// How many resume points a dispute over a workload of `per_unit` bytes may keep.
///
/// Zero is a legitimate answer and not a failure: [`dispute::Replay`] documents a budget of zero
/// as replaying from the start every time, which is `log₂(n)` times slower and produces
/// **identical answers**. A volunteer too small to hold checkpoints argues slowly. A volunteer
/// that ran out of memory holding them argues not at all, and is convicted by abandonment.
///
/// [`dispute::Replay`]: cairn_runtime::dispute::Replay
#[must_use]
pub fn checkpoints(survey: &Survey, per_unit: u64) -> usize {
    if per_unit == 0 {
        return DEFAULT_CHECKPOINT_BUDGET;
    }
    let affordable = usize::try_from(survey.memory.bytes() / 4 / per_unit).unwrap_or(usize::MAX);
    affordable.min(DEFAULT_CHECKPOINT_BUDGET)
}

/// What a workload's declared page count costs, in bytes.
///
/// The declared **maximum** rather than the initial size, because that is what the volunteer has
/// committed to being able to hold: a workload that grows to its ceiling mid-unit must not be the
/// moment the machine discovers it over-committed.
#[must_use]
pub const fn per_unit_bytes(declared_pages: u32) -> u64 {
    (declared_pages as u64).saturating_mul(PAGE)
}

/// A pool of memory that units claim before executing and release afterwards.
///
/// Concurrency falls out of this rather than being set: with a 1 GiB pool, sixteen threads run
/// sixteen 64 MiB units or two 512 MiB ones, and nobody had to know in advance which workload the
/// grid would serve. Threads that cannot be paid block until one finishes.
///
/// A [`Mutex`] and a [`Condvar`], because that is what this is. The same pair the coordinator's
/// `Desk` uses for the same reason: a blocking waiter and a bounded resource.
#[derive(Debug)]
pub struct Allowance {
    total: u64,
    free: Mutex<u64>,
    released: Condvar,
}

impl Allowance {
    /// A pool of `total` bytes.
    #[must_use]
    pub fn new(total: u64) -> Arc<Self> {
        Arc::new(Self {
            total,
            free: Mutex::new(total),
            released: Condvar::new(),
        })
    }

    /// The pool's size, for printing.
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.total
    }

    /// Claim `want` bytes, blocking until they are free. The claim is released when dropped.
    ///
    /// # A unit larger than the whole pool runs anyway
    ///
    /// It runs **alone**, because the claim is clamped to the pool's size rather than refused.
    /// Refusing would mean a machine whose budget is under one unit donates nothing at all, which
    /// is worse than letting the operating system make that call — it has the actual page tables
    /// and this has an estimate from a header field. What must never happen is *two* such units
    /// at once, and clamping rather than refusing is exactly what prevents that.
    ///
    /// # Panics
    ///
    /// Never in practice. A poisoned lock means another volunteer thread panicked mid-claim, and
    /// this returns an unclaimed permit rather than propagating: the failure has already been
    /// reported by the thread that caused it, and taking the whole volunteer down over it would
    /// turn one lost unit into a lost machine.
    pub fn claim(self: &Arc<Self>, want: u64) -> Claim {
        let need = want.min(self.total).max(1);
        let Ok(mut free) = self.free.lock() else {
            return Claim {
                pool: Arc::clone(self),
                held: 0,
            };
        };
        while *free < need {
            let Ok(waited) = self.released.wait(free) else {
                return Claim {
                    pool: Arc::clone(self),
                    held: 0,
                };
            };
            free = waited;
        }
        *free -= need;
        Claim {
            pool: Arc::clone(self),
            held: need,
        }
    }

    /// Bytes not currently claimed.
    ///
    /// Test-only, and deliberately so. The invariant worth checking is about the pool, not about
    /// any one claim, and there is no honest way for running code to read this number and act on
    /// it: by the time a caller saw it, it would be stale. [`Self::claim`] is the whole interface.
    #[cfg(test)]
    #[must_use]
    pub fn free(&self) -> u64 {
        self.free.lock().map_or(0, |free| *free)
    }
}

/// Memory held for as long as a unit is executing.
///
/// Releasing on `Drop` and not on a method call, so that a unit which traps, fails to compile, or
/// panics gives its memory back on the way out. A leaked claim is a volunteer that slowly stops
/// taking work for no visible reason, which is the hardest kind of bug to be told about.
#[derive(Debug)]
pub struct Claim {
    pool: Arc<Allowance>,
    held: u64,
}

impl Drop for Claim {
    fn drop(&mut self) {
        if let Ok(mut free) = self.pool.free.lock() {
            *free = free.saturating_add(self.held).min(self.pool.total);
        }
        // Notify all rather than one: waiters want different amounts, and waking the smallest
        // waiter is not something a condvar can be asked to do. Waking one that still cannot be
        // paid would leave a satisfiable waiter asleep behind it.
        self.pool.released.notify_all();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use cairn_runtime::validate::Limits;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    const MIB: u64 = 1024 * 1024;

    fn machine(cores: usize, memory: u64) -> Survey {
        Survey {
            cores,
            memory: Budget::Stated(memory),
        }
    }

    #[test]
    fn one_core_is_left_for_the_person_who_owns_the_machine() {
        assert_eq!(threads(&machine(16, 16 * 1024 * MIB), None), 15);
        assert_eq!(threads(&machine(8, 16 * 1024 * MIB), None), 7);
    }

    #[test]
    fn a_single_core_machine_still_donates_one_thread() {
        assert_eq!(threads(&machine(1, 16 * 1024 * MIB), None), 1);
        assert_eq!(threads(&machine(2, 16 * 1024 * MIB), None), 1);
    }

    #[test]
    fn the_operators_flag_can_only_lower_the_count() {
        let big = machine(64, 256 * 1024 * MIB);
        assert_eq!(threads(&big, Some(4)), 4);
        // Asking for more than the machine has, or more than the blast-radius cap allows, gets
        // the cap. This is the property that makes the flag safe to expose at all.
        assert_eq!(threads(&big, Some(1000)), MOST_UNITS_AT_ONCE);
        assert_eq!(threads(&big, None), MOST_UNITS_AT_ONCE);
    }

    #[test]
    fn a_dispute_may_hold_checkpoints_only_out_of_its_own_quarter() {
        // 16 GiB machine, a workload declaring 64 MiB: a quarter is 4 GiB, which is 64 clones,
        // so the runtime's own budget is what binds.
        assert_eq!(
            checkpoints(&machine(16, 16 * 1024 * MIB), 64 * MIB),
            DEFAULT_CHECKPOINT_BUDGET
        );
        // 2 GiB machine, a workload declaring the 256 MiB maximum: a quarter is 512 MiB, which
        // is two clones. Holding 32 would be 8 GiB on a machine that has 2.
        assert_eq!(checkpoints(&machine(16, 2 * 1024 * MIB), 256 * MIB), 2);
    }

    #[test]
    fn a_machine_too_small_to_checkpoint_argues_without_checkpoints() {
        // Zero is a slower argument, not a lost one — Replay documents it as identical in
        // answers and `log2(n)` times slower. The alternative is dying while holding clones.
        assert_eq!(checkpoints(&machine(4, 256 * MIB), 256 * MIB), 0);
    }

    #[test]
    fn declared_pages_convert_through_the_only_admitted_page_size() {
        assert_eq!(per_unit_bytes(1), 64 * 1024);
        // Read from the admission gate rather than written down again, so that raising the
        // network's ceiling shows up here as the worst case a volunteer has to budget for.
        assert_eq!(
            per_unit_bytes(Limits::default().max_memory_pages),
            256 * MIB
        );
    }

    #[test]
    fn the_claimed_memory_of_running_units_never_exceeds_the_budget() {
        let pool = Allowance::new(100);
        let first = pool.claim(60);
        assert_eq!(pool.free(), 40);
        let second = pool.claim(40);
        assert_eq!(pool.free(), 0);
        drop(first);
        assert_eq!(pool.free(), 60);
        drop(second);
        assert_eq!(pool.free(), 100);
    }

    #[test]
    fn a_unit_bigger_than_the_whole_budget_runs_alone_rather_than_never() {
        let pool = Allowance::new(100);
        let held = pool.claim(4096);
        // Clamped to the pool, so it runs — and so nothing else can run beside it.
        assert_eq!(pool.free(), 0);
        drop(held);
        assert_eq!(pool.free(), 100);
    }

    #[test]
    fn a_thread_that_cannot_be_paid_waits_for_one_that_can_pay_it_back() {
        let pool = Allowance::new(100);
        let held = pool.claim(80);
        let admitted = Arc::new(AtomicUsize::new(0));

        std::thread::scope(|scope| {
            let waiting = {
                let pool = Arc::clone(&pool);
                let admitted = Arc::clone(&admitted);
                scope.spawn(move || {
                    let claim = pool.claim(50);
                    admitted.fetch_add(1, Ordering::SeqCst);
                    claim
                })
            };

            // 50 does not fit beside 80. Not a proof of blocking — no timing test is — but it
            // fails loudly if the pool ever hands out memory it does not have, which is the
            // failure that matters.
            std::thread::sleep(Duration::from_millis(50));
            assert_eq!(admitted.load(Ordering::SeqCst), 0, "over-committed");

            drop(held);
            let claim = waiting.join().expect("waiting thread panicked");
            assert_eq!(admitted.load(Ordering::SeqCst), 1);
            assert_eq!(pool.free(), 50);
            drop(claim);
        });
    }

    #[test]
    fn a_trapped_unit_gives_its_memory_back() {
        let pool = Allowance::new(100);
        let result = std::panic::catch_unwind({
            let pool = Arc::clone(&pool);
            move || {
                let _claim = pool.claim(100);
                panic!("a work unit is a program somebody else wrote");
            }
        });
        assert!(result.is_err());
        assert_eq!(pool.free(), 100, "a claim leaked past a panic");
    }
}
