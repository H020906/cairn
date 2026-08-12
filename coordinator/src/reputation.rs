//! What the coordinator has observed about each volunteer, and what it does with that.
//!
//! # Why this exists at all, and why it is this small
//!
//! [ADR-0001](../../docs/adr/0001-verification-by-dispute-not-replication.md) is built on three
//! things, and until now only one of them was real. Its cost model is `1 + s + c + r`, where `r`
//! is the replication rate — a dial that existed — and `c` is the **canary rate**, which did not.
//! Without canaries, nothing in Cairn ever caught a cheat *by itself*: a wrong answer was found
//! only when a second volunteer happened to disagree with it, on the ten percent of units that
//! were replicated.
//!
//! `grid.rs` used to carry a warning that inventing a scoring rule with no real workers to score
//! would be fiction. That warning is right and this module is written to stay inside it:
//!
//! - The **posterior** is not invented. ADR-0001 asks for "a per-worker posterior on *returns
//!   correct results*", and the standard estimator for a sequence of pass/fail observations is
//!   the Beta-Binomial mean. That is what this computes.
//! - The **thresholds** are invented, so they are not constants — they are dials on [`Policy`]
//!   with their defaults stated and their effect measured, exactly as `--replicate` is.
//! - The **weights** are the one genuine judgement, and there is precisely one of them: a proven
//!   lie counts for more than a failed canary. ADR-0001 asks for that too — "the reputation
//!   penalty for silence is materially different from the penalty for a proven wrong state
//!   transition" — and the number is a dial rather than a belief. **A refuted result deliberately
//!   did not add a second one.** When the referee re-executes a disputed unit it ends up holding
//!   the true answer, which is the same position a canary puts it in, so the two are weighed
//!   alike; inventing a dial to make them differ would have been a belief with nothing behind it.
//!
//! # Integers, not floats
//!
//! The posterior is a ratio of counts and it is kept as one, reported in permille. This
//! workspace denies `float_cmp` because a float comparison in a consensus decision is a way to
//! make two coordinators disagree; dispatch policy is not consensus-critical, but there is no
//! reason to introduce a float where a fraction of two integers is exact and says the same thing.

use std::collections::HashMap;

/// Observations about one volunteer.
///
/// Counts only. What they *mean* is [`Policy`]'s business, so that changing the policy never
/// means rewriting history.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Record {
    /// Canaries answered correctly.
    pub passed: u32,
    /// Canaries answered incorrectly. **Direct evidence of a wrong answer**, needing no dispute
    /// and no second volunteer: the coordinator already knew what the answer was.
    pub failed: u32,
    /// Results the referee re-executed and found wrong.
    ///
    /// **The same kind of evidence as [`failed`](Self::failed), arrived at from the other end.**
    /// A canary is a unit whose answer the coordinator knew in advance; a refutation is a unit
    /// whose answer it worked out afterwards, because two volunteers disagreed and neither could
    /// argue. Either way the coordinator holds the true answer and this worker's differs from it,
    /// so the two count the same in the posterior and are weighed as one failed check each.
    ///
    /// **Kept as its own counter anyway, for two reasons.** The canary measurement in
    /// `tests/canaries.rs` reports units-until-caught and would be contaminated by catches that
    /// no canary made; and an operator looking at [`Standing::ProvenWrong`] should be able to see
    /// which mechanism did the catching, because they cost very different amounts.
    ///
    /// **Not counted as a lie.** The referee proved the *result* wrong and proved nothing about
    /// intent: a browser volunteer whose engine diverges returns a wrong answer honestly, and
    /// that is the failure this whole project is arranged to avoid punishing. Only losing a
    /// bisection shows a party corrupting its own replay, and only that is [`lied`](Self::lied).
    pub refuted: u32,
    /// Disputes this worker was proven to have lied in.
    pub lied: u32,
    /// Disputes this worker abandoned by going quiet.
    ///
    /// Kept apart from [`lied`](Self::lied) because ADR-0001 asks for exactly that distinction:
    /// a volunteer that stopped answering may have closed a laptop, and convicting it of fraud
    /// on absence alone is how an honest volunteer is punished for having a life.
    pub silent: u32,
    /// Results accepted. Not evidence of honesty — almost every unit is accepted after a single
    /// execution, so this counts work done rather than trust earned.
    pub accepted: u64,
}

impl Record {
    /// The Beta-Binomial posterior mean that this worker returns correct results, in permille.
    ///
    /// `(α₀ + passed) / (α₀ + β₀ + passed + weighted failures)`, with the prior from [`Policy`].
    ///
    /// The prior is what makes a brand-new worker untrusted without any special case: with
    /// `Beta(1, 1)` and no observations the posterior is 500‰, which is below any sensible
    /// threshold, so a worker earns trust by passing canaries rather than by arriving.
    #[must_use]
    pub const fn posterior_permille(&self, policy: &Policy) -> u32 {
        let good = policy.prior_good as u64 + self.passed as u64;
        let bad = policy.prior_bad as u64
            + self.failed as u64
            + self.refuted as u64
            + self.lied as u64 * policy.weight_of_a_lie as u64
            + self.silent as u64 * policy.weight_of_silence as u64;
        let total = good + bad;
        if total == 0 {
            return 0;
        }
        ((good * 1000) / total) as u32
    }

    /// Whether anything has been proven against this worker.
    ///
    /// Separate from the posterior on purpose. A failed canary, a refuted result or a proven lie
    /// is not evidence to be weighed against other evidence — it is a wrong answer the
    /// coordinator *knows* was wrong, and no amount of subsequent good behaviour makes it not
    /// have happened.
    #[must_use]
    pub const fn is_proven_wrong(&self) -> bool {
        self.failed > 0 || self.refuted > 0 || self.lied > 0
    }
}

/// The dials. Everything here is policy, and none of it is measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// Beta prior, successes. `1` with [`prior_bad`](Self::prior_bad) `1` is the uniform prior:
    /// before any evidence, a worker is equally likely to be anything.
    pub prior_good: u32,
    /// Beta prior, failures.
    pub prior_bad: u32,
    /// How many failed canaries a proven lie is worth.
    ///
    /// The one real judgement in this file. ADR-0001 asks for a lie to cost more than silence;
    /// a lie is also worse than a failed canary, because a failed canary can be broken hardware
    /// and losing a bisection cannot — the party had to corrupt its own replay to get there.
    pub weight_of_a_lie: u32,
    /// How many failed canaries an abandoned dispute is worth.
    ///
    /// Deliberately the smallest non-zero weight. A volunteer that went quiet may have closed a
    /// laptop, and ADR-0001 is explicit that absence must not be treated as fraud.
    pub weight_of_silence: u32,
    /// Posterior below which a worker is not trusted, in permille.
    pub trusted_above_permille: u32,
    /// Canaries a worker must pass before it can be trusted at all, whatever its posterior.
    ///
    /// **Non-binding at the defaults, and deliberately kept anyway.** With `Beta(1,1)` and
    /// [`trusted_above_permille`](Self::trusted_above_permille) at 900, the posterior alone
    /// already demands nine clean canaries — `(1+9)/(2+9) = 909‰` is the first that clears it —
    /// so this floor never fires. It exists for the operator who lowers the threshold: without
    /// it, `trusted_above_permille: 600` would let a single honest answer (667‰) buy trust, and
    /// a cheat would pay one unit for the reduced sampling rate.
    ///
    /// An earlier version of this comment claimed the floor was what stopped one canary from
    /// buying trust at the *default* threshold. That was wrong, and the test below is written
    /// to make the real number visible rather than to assert a constant.
    pub proving_canaries: u32,
    /// Canary rate for a trusted worker, in permille of the units it is given.
    ///
    /// 30‰ is ADR-0001's `c ≈ 0.03`, and it is the number its cost model was written around.
    pub canaries_when_trusted: u32,
    /// Canary rate for a worker that is not trusted, in permille.
    pub canaries_when_not: u32,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            prior_good: 1,
            prior_bad: 1,
            weight_of_a_lie: 20,
            weight_of_silence: 1,
            trusted_above_permille: 900,
            proving_canaries: 3,
            canaries_when_trusted: 30,
            canaries_when_not: 250,
        }
    }
}

/// What the coordinator has observed, and what it concludes.
#[derive(Debug, Clone, Default)]
pub struct Reputation {
    workers: HashMap<String, Record>,
    policy: Policy,
}

/// Why a worker is or is not trusted, in a form that can be printed and tested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    /// Never seen enough of this worker to say. Gets canaries at the untrusted rate.
    Unproven {
        /// How many more canaries it must pass.
        needs: u32,
    },
    /// Has passed enough canaries and nothing is proven against it.
    Trusted {
        /// Posterior in permille.
        permille: u32,
    },
    /// Returned an answer the coordinator knew was wrong, or lost a bisection.
    ///
    /// Not a ban. Cairn has no penalties — see `grid.rs` — so what this changes is how often the
    /// worker is checked, and nothing else. Deciding to *exclude* a volunteer is a policy with
    /// consequences for real people, and it needs an operator, not a constant.
    ProvenWrong {
        /// Canaries it got wrong.
        failed: u32,
        /// Results the referee re-executed and found wrong.
        refuted: u32,
        /// Disputes it was proven to have lied in.
        lied: u32,
    },
}

impl Reputation {
    /// An empty history under a chosen policy.
    #[must_use]
    pub fn new(policy: Policy) -> Self {
        Self {
            workers: HashMap::new(),
            policy,
        }
    }

    /// The dials in force.
    #[must_use]
    pub const fn policy(&self) -> &Policy {
        &self.policy
    }

    /// What has been observed about one worker. Absent means nothing yet.
    #[must_use]
    pub fn record(&self, worker: &str) -> Record {
        self.workers.get(worker).copied().unwrap_or_default()
    }

    /// Every worker seen, in no particular order.
    pub fn workers(&self) -> impl Iterator<Item = (&String, &Record)> {
        self.workers.iter()
    }

    /// Where a worker stands.
    #[must_use]
    pub fn standing(&self, worker: &str) -> Standing {
        let record = self.record(worker);
        if record.is_proven_wrong() {
            return Standing::ProvenWrong {
                failed: record.failed,
                refuted: record.refuted,
                lied: record.lied,
            };
        }
        if record.passed < self.policy.proving_canaries {
            return Standing::Unproven {
                needs: self.policy.proving_canaries - record.passed,
            };
        }
        let permille = record.posterior_permille(&self.policy);
        if permille > self.policy.trusted_above_permille {
            Standing::Trusted { permille }
        } else {
            Standing::Unproven { needs: 1 }
        }
    }

    /// How often this worker should be handed a canary, in permille.
    ///
    /// This is the whole of "selective" in ADR-0001's *selective replication*, moved onto the
    /// sampling mechanism instead of the replication one — see
    /// [ADR-0015](../../docs/adr/0015-canaries-are-what-catch-a-cheat.md) for why. A trusted
    /// worker is checked at the rate the cost model was written around; everybody else is
    /// checked hard until it becomes one, or until it fails.
    #[must_use]
    pub fn canary_permille(&self, worker: &str) -> u32 {
        match self.standing(worker) {
            Standing::Trusted { .. } => self.policy.canaries_when_trusted,
            Standing::Unproven { .. } | Standing::ProvenWrong { .. } => {
                self.policy.canaries_when_not
            }
        }
    }

    /// Record a canary answered correctly.
    pub fn passed_a_canary(&mut self, worker: &str) {
        self.entry(worker).passed += 1;
    }

    /// Record a canary answered incorrectly.
    pub fn failed_a_canary(&mut self, worker: &str) {
        self.entry(worker).failed += 1;
    }

    /// Record a result the referee re-executed and found wrong.
    ///
    /// This is the route that had no way to report anything until now: two volunteers disagree,
    /// neither can argue, the referee executes the unit itself and knows which of them is wrong —
    /// and the answer went into the unit's outcome and nowhere else. ADR-0015 named it as the
    /// most valuable single gap left in that design, and this is the other end of it.
    pub fn refuted(&mut self, worker: &str) {
        self.entry(worker).refuted += 1;
    }

    /// Record a dispute this worker was proven to have lied in.
    pub fn lied(&mut self, worker: &str) {
        self.entry(worker).lied += 1;
    }

    /// Record a dispute this worker abandoned.
    pub fn went_silent(&mut self, worker: &str) {
        self.entry(worker).silent += 1;
    }

    /// Record an accepted result. Work done, not trust earned.
    pub fn accepted(&mut self, worker: &str) {
        self.entry(worker).accepted += 1;
    }

    fn entry(&mut self, worker: &str) -> &mut Record {
        self.workers.entry(worker.to_owned()).or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_worker_nobody_has_seen_is_not_trusted() {
        // The prior does this, with no special case for newness: `Beta(1,1)` with no observations
        // is 500 permille, and nothing reasonable trusts a coin flip.
        let reputation = Reputation::new(Policy::default());
        assert_eq!(
            reputation
                .record("stranger")
                .posterior_permille(&Policy::default()),
            500
        );
        assert!(matches!(
            reputation.standing("stranger"),
            Standing::Unproven { .. }
        ));
    }

    #[test]
    fn trust_costs_nine_clean_canaries_and_the_number_comes_from_the_prior() {
        // Not a constant anybody chose. With `Beta(1,1)` and a threshold of 900‰, the posterior
        // after `p` clean canaries is `(1 + p) / (2 + p)`, which first exceeds 900‰ at p = 9:
        // 8 gives exactly 900‰ and does not clear it, 9 gives 909‰ and does.
        //
        // This test walks until trust arrives and reports where, so that changing the prior or
        // the threshold shows up here as the new number rather than as a failure to explain.
        let mut reputation = Reputation::new(Policy::default());
        let mut passed = 0;
        while !matches!(reputation.standing("alice"), Standing::Trusted { .. }) {
            reputation.passed_a_canary("alice");
            passed += 1;
            assert!(passed < 100, "trust never arrived");
        }

        assert_eq!(passed, 9, "the posterior arithmetic moved");
        assert_eq!(
            Record {
                passed: 8,
                ..Record::default()
            }
            .posterior_permille(&Policy::default()),
            900,
            "eight canaries reach the threshold exactly, and `>` is what keeps them out"
        );
    }

    #[test]
    fn the_proving_floor_is_what_stops_a_lowered_threshold_from_selling_trust_cheaply() {
        // The floor does nothing at the defaults. It exists for an operator who decides nine
        // canaries is too slow: at 600‰ a single passed canary reaches 667‰, and without the
        // floor one honest answer would buy the reduced sampling rate a cheat wants.
        let lenient = Policy {
            trusted_above_permille: 600,
            ..Policy::default()
        };
        let mut reputation = Reputation::new(lenient);
        reputation.passed_a_canary("alice");

        assert!(reputation.record("alice").posterior_permille(&lenient) > 600);
        assert_eq!(
            reputation.standing("alice"),
            Standing::Unproven { needs: 2 },
            "one canary bought trust under a lenient threshold"
        );
    }

    #[test]
    fn one_wrong_answer_the_coordinator_already_knew_is_not_outweighed_by_good_behaviour() {
        // The reason `is_proven_wrong` is separate from the posterior. A worker that failed a
        // canary and then passed a thousand has a posterior above any threshold, and it still
        // returned an answer that was known to be wrong.
        let mut reputation = Reputation::new(Policy::default());
        reputation.failed_a_canary("mallory");
        for _ in 0..1000 {
            reputation.passed_a_canary("mallory");
        }

        assert!(
            reputation
                .record("mallory")
                .posterior_permille(&Policy::default())
                > 990
        );
        assert_eq!(
            reputation.standing("mallory"),
            Standing::ProvenWrong {
                failed: 1,
                refuted: 0,
                lied: 0
            },
            "a thousand right answers un-did one known-wrong one"
        );
    }

    #[test]
    fn a_refuted_result_costs_exactly_what_a_failed_canary_costs() {
        // The claim in `Record::refuted`'s documentation, as arithmetic. Two workers with
        // identical histories except for *how* the coordinator came to know they were wrong must
        // end up indistinguishable, because what it knows about them is the same thing.
        let policy = Policy::default();
        let mut by_canary = Reputation::new(policy);
        let mut by_referee = Reputation::new(policy);
        for _ in 0..20 {
            by_canary.passed_a_canary("a");
            by_referee.passed_a_canary("b");
        }
        by_canary.failed_a_canary("a");
        by_referee.refuted("b");

        assert_eq!(
            by_canary.record("a").posterior_permille(&policy),
            by_referee.record("b").posterior_permille(&policy),
            "the two kinds of known-wrong answer drifted apart in the posterior"
        );
        assert!(by_referee.record("b").is_proven_wrong());
        assert_eq!(
            by_referee.standing("b"),
            Standing::ProvenWrong {
                failed: 0,
                refuted: 1,
                lied: 0
            },
            "an operator cannot see which mechanism caught this"
        );
    }

    #[test]
    fn a_refuted_volunteer_starts_being_checked_hard_again() {
        // What makes the counter more than bookkeeping. Twenty clean canaries buy the reduced
        // rate the cost model is written around; one answer the referee could show was wrong
        // takes it away, and the worker goes back to being checked at the untrusted rate.
        //
        // Written as a comparison against the rate this worker actually held rather than against
        // the constant, so that changing a default shows up here as a different number instead of
        // as a test that quietly stops meaning anything.
        let policy = Policy::default();
        let mut reputation = Reputation::new(policy);
        for _ in 0..20 {
            reputation.passed_a_canary("volunteer");
        }
        let while_trusted = reputation.canary_permille("volunteer");
        assert!(matches!(
            reputation.standing("volunteer"),
            Standing::Trusted { .. }
        ));

        reputation.refuted("volunteer");

        assert!(
            reputation.canary_permille("volunteer") > while_trusted,
            "a refuted volunteer is still checked as rarely as a trusted one"
        );
    }

    #[test]
    fn being_refuted_costs_far_less_than_losing_a_bisection() {
        // The distinction the `refuted` counter exists to preserve. Being refuted says the
        // *result* was wrong; losing a bisection says the party corrupted its own replay to
        // defend it. A browser volunteer with a divergent engine reaches the first honestly and
        // cannot reach the second at all, so collapsing them would put the project's own worst
        // failure — convicting an honest volunteer — one code change away.
        let policy = Policy::default();
        let mut refuted = Reputation::new(policy);
        let mut liar = Reputation::new(policy);
        for _ in 0..20 {
            refuted.passed_a_canary("unlucky");
            liar.passed_a_canary("liar");
        }
        refuted.refuted("unlucky");
        liar.lied("liar");

        let unlucky_score = refuted.record("unlucky").posterior_permille(&policy);
        let liar_score = liar.record("liar").posterior_permille(&policy);
        assert!(
            unlucky_score > liar_score,
            "a refuted result {unlucky_score} should cost less than a lie {liar_score}"
        );
        // Both are still proven wrong, though: the coordinator knows each of them returned an
        // answer it can show was not the answer. What differs is the weight, not the fact.
        assert!(refuted.record("unlucky").is_proven_wrong());
        assert!(liar.record("liar").is_proven_wrong());
    }

    #[test]
    fn silence_costs_far_less_than_a_proven_lie() {
        // ADR-0001 is explicit that these must differ: a volunteer that stopped answering may
        // have closed a laptop, and convicting it of fraud on absence alone punishes an honest
        // volunteer for having a life.
        let policy = Policy::default();
        let mut quiet = Reputation::new(policy);
        let mut liar = Reputation::new(policy);
        for _ in 0..20 {
            quiet.passed_a_canary("quiet");
            liar.passed_a_canary("liar");
        }
        quiet.went_silent("quiet");
        liar.lied("liar");

        let quiet_score = quiet.record("quiet").posterior_permille(&policy);
        let liar_score = liar.record("liar").posterior_permille(&policy);
        assert!(
            quiet_score > liar_score,
            "silence {quiet_score} should cost less than a lie {liar_score}"
        );
        assert!(
            matches!(quiet.standing("quiet"), Standing::Trusted { .. }),
            "twenty good units and one closed laptop must not cost a volunteer its standing"
        );
        assert!(matches!(
            liar.standing("liar"),
            Standing::ProvenWrong { lied: 1, .. }
        ));
    }
}
