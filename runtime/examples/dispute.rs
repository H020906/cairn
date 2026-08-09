//! The whole idea, round by round, in one runnable file.
//!
//! ```text
//! cargo run --example dispute
//! ```
//!
//! Two volunteers return different answers for the same work unit. **Nobody re-runs the job.**
//! Instead the two are asked to commit to their own executions, a binary search narrows the
//! disagreement to the single machine instruction where they first diverged, and the
//! coordinator executes *that one instruction* to find out which of them was lying.
//!
//! Everything below is the real implementation — `canon`, `Machine`, `Challenge`, `adjudicate`.
//! Nothing here is a mock. What this file adds is that it drives the bisection **by hand and
//! prints every round**, which [`dispute::resolve`] does not: `resolve` is the same loop with
//! the printing taken out.
//!
//! # About the liar
//!
//! The dishonest party is simulated by perturbing the roots it reports from a chosen step
//! onward. That is not a shortcut — it is exactly what a party whose execution went differently
//! from that step looks like, because **the coordinator never sees anything but roots.** A liar
//! is fully characterised by the hashes it reports, which is the whole reason the protocol can
//! be this cheap.

// An example is a document. It uses `unwrap` where a failure means the example itself is
// broken, and prints rather than returns.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::print_stdout)]

use cairn_runtime::canon::{self, Config};
use cairn_runtime::dispute::{self, Absent, Challenge, Claimant, Judgment, Round, Step, Verdict};
use cairn_runtime::engine::image;
use cairn_runtime::engine::machine::{Limits, Machine};
use cairn_runtime::merkle::Hash;
use cairn_runtime::validate;

/// A work unit: a loop of independent arithmetic with one answer at the end.
///
/// Small enough that the whole execution fits in a few thousand instructions, so the bisection
/// below is a dozen rounds rather than twenty and every one of them is legible.
const WORKLOAD: &str = r#"
    (module
      (import "cairn" "output" (func $output (param i32 i32)))
      (memory (export "memory") 1 1)
      (func (export "cairn_run") (local $i i32) (local $acc i64)
        (block $done
          (loop $again
            (br_if $done (i32.ge_u (local.get $i) (i32.const 200)))
            (local.set $acc
              (i64.add (local.get $acc)
                       (i64.mul (i64.extend_i32_u (local.get $i))
                                (i64.extend_i32_u (local.get $i)))))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $again)))
        (i64.store (i32.const 0) (local.get $acc))
        (call $output (i32.const 0) (i32.const 8))))
"#;

/// A party that computed correctly up to `diverged_at` and then did something else.
///
/// Wraps an honest replay and perturbs every root from that step onward. From the
/// coordinator's side this is indistinguishable from a worker whose CPU dropped a bit, or one
/// that stopped computing and started inventing — and the protocol is designed not to care
/// which, because it cannot tell and does not need to.
struct Liar<'a> {
    honest: dispute::Replay<'a>,
    diverged_at: Step,
}

impl Claimant for Liar<'_> {
    fn root_at(&mut self, step: Step) -> Result<Option<Hash>, Absent> {
        let truth = self.honest.root_at(step)?;
        if step.get() < self.diverged_at.get() {
            return Ok(truth);
        }
        // One bit. A liar does not have to be dramatic to be caught — the commitment is a hash,
        // so any difference at all is the same size of difference.
        Ok(truth.map(|mut root| {
            root[0] ^= 1;
            root
        }))
    }
}

fn main() {
    rule("Cairn — settling a disagreement by executing one instruction");

    // --- 1. the coordinator prepares the unit -----------------------------------------------
    step_heading(1, "The coordinator prepares the work unit");
    println!(
        "  Instrumentation happens once, at registration. Every volunteer runs these exact\n\
         \x20 bytes, and their hash is what identifies the unit."
    );

    let source = wat::parse_str(WORKLOAD).expect("workload should assemble");
    validate::validate_submitted(&source, validate::Limits::default())
        .expect("workload should be admissible");
    let unit = canon::instrument(&source, Config::dispute_path()).expect("should instrument");
    let decoded = image::decode(&unit).expect("should decode");

    println!();
    println!("    unit id   {}", short(&blake3_of(&unit)));
    println!("    bytes     {}", unit.len());

    // --- 2. the honest execution ------------------------------------------------------------
    step_heading(2, "A volunteer executes it and returns an answer");

    let input = Vec::new();
    let mut machine =
        Machine::new(&decoded, input.clone(), Limits::default()).expect("should instantiate");
    let trace = machine.run().expect("workload should not trap");
    let length = Step::new(trace.steps);

    println!("    steps     {}", trace.steps);
    println!("    fuel      {}", trace.fuel.get());
    println!(
        "    answer    {}  = {}",
        hex(&trace.output),
        u64::from_le_bytes(trace.output.clone().try_into().expect("an i64 result"))
    );
    println!("    final     {}", short(&trace.final_root));
    println!();
    println!(
        "  On the honest path a volunteer returns only the answer — no trace, no proof. The\n\
         \x20 roots exist because *this* execution is under full instrumentation, which is what\n\
         \x20 a challenged party re-runs. See docs/adr/0005."
    );

    // --- 3. the disagreement ----------------------------------------------------------------
    // Late on purpose: it is the expensive shape, so the bisection has to work for its answer
    // rather than settling in two rounds.
    let diverged_at = Step::new(trace.steps * 4 / 5);

    step_heading(3, "A second volunteer returns something else");
    println!(
        "  Its execution matches up to instruction {}, and differs from there on. Nobody knows\n\
         \x20 that yet — all the coordinator sees is two different answers.",
        diverged_at.get()
    );

    let mut honest = dispute::Replay::new(&decoded, input.clone(), Limits::default());
    let mut liar = Liar {
        honest: dispute::Replay::new(&decoded, input.clone(), Limits::default()),
        diverged_at,
    };

    // --- 4. the bisection -------------------------------------------------------------------
    step_heading(4, "Neither is re-run. They play a bisection game.");
    println!(
        "  Each round the coordinator names one step and asks both parties for the state root\n\
         \x20 there. Agreement moves the floor up, disagreement moves the ceiling down. The\n\
         \x20 coordinator executes nothing.\n"
    );

    let verdict = bisect(&mut honest, &mut liar, length);

    println!();
    println!(
        "  {} rounds for a {}-instruction execution — ⌈log₂ n⌉, and that is the whole cost to\n\
         \x20 the coordinator so far. A trillion-instruction unit would take {} more.",
        verdict.rounds,
        length.get(),
        40 - verdict.rounds
    );

    // --- 5. adjudication --------------------------------------------------------------------
    step_heading(5, "The coordinator executes exactly one instruction");
    println!(
        "  The two agree on the state entering instruction {} and disagree on the state\n\
         \x20 leaving it. Exactly one of them can be right.\n",
        verdict.divergence.get()
    );
    println!("    before          {}", opt_short(verdict.agreed_root));
    println!("    first claims    {}", opt_short(verdict.first_claim));
    println!("    second claims   {}", opt_short(verdict.second_claim));

    // The witness is the agreed state itself rather than a hash of it: small state whole,
    // memory as only the pages that instruction touches, with proofs. It must come from an
    // execution of the unit *as assigned*, which is what makes this a judgement rather than a
    // third opinion.
    let mut judge =
        Machine::new(&decoded, input.clone(), Limits::default()).expect("should instantiate");
    for _ in 0..verdict.divergence.get() {
        if judge.step().is_err() {
            break;
        }
    }
    let witness = judge.witness_for_next_step();

    let judgment = dispute::adjudicate(&decoded, &verdict, &witness, &input, Limits::default())
        .expect("should adjudicate");

    println!();
    println!(
        "    witness         {}, {}, {}",
        plural(witness.operand_stack.len(), "operand"),
        plural(witness.frames.len(), "frame"),
        plural(witness.pages.len(), "memory page"),
    );
    println!("    verdict         {}", describe(&judgment));
    println!();
    println!(
        "  Zero pages is not a mistake. The disputed instruction is arithmetic — it reads two\n\
         \x20 operands and writes one, and touches no memory at all, which is true of most\n\
         \x20 instructions in most programs. A witness carries the small state whole and\n\
         \x20 memory as only the pages one instruction can reach: none here, one 64 KiB page\n\
         \x20 for an ordinary load or store, and more only for a `memory.fill` long enough to\n\
         \x20 cross a page boundary."
    );

    // --- what to take away ------------------------------------------------------------------
    rule("What just happened");
    println!(
        "  A {}-instruction disagreement was settled by executing ONE instruction.\n",
        length.get()
    );
    println!("    the coordinator executed     1 instruction");
    println!("    messages exchanged           {}", verdict.rounds);
    println!(
        "    state the judge needed       {}, {}, {}",
        plural(witness.operand_stack.len(), "operand"),
        plural(witness.frames.len(), "frame"),
        plural(witness.pages.len(), "memory page"),
    );
    println!();
    println!(
        "  None of those three numbers grows when the disputed execution does. That is the\n\
         \x20 claim the whole project rests on, and `cargo bench` measures it: 21k steps take 15\n\
         \x20 rounds, 2.1M steps take 21.\n"
    );
    println!(
        "  What it costs the *parties* is a different question with a worse answer — they have\n\
         \x20 to re-execute under Cairn's interpreter to produce roots at all, and that is\n\
         \x20 37×–142× slower than the engine they did the work on. See docs/adr/0008."
    );
}

/// Drive the bisection by hand, printing every round.
///
/// This is [`dispute::resolve`] with the printing left in. Keeping the loop visible is the
/// point of the example: the state machine is eleven lines and it is the entire protocol.
fn bisect(first: &mut impl Claimant, second: &mut impl Claimant, length: Step) -> Verdict {
    let mut challenge = Challenge::open(length).expect("a non-empty execution");

    println!("    round   bracket                 ask at      parties      bracket becomes");
    println!("    ─────   ─────────────────────   ─────────   ──────────   ─────────────────");

    loop {
        let (low, high) = challenge.bounds();
        match challenge.round() {
            Round::Ask { step } => {
                let a = first.root_at(step).expect("present");
                let b = second.root_at(step).expect("present");
                challenge.record(a, b);
                let (new_low, new_high) = challenge.bounds();
                println!(
                    "    {:>5}   [{:>8}, {:>8}]   {:>9}   {:<10}   [{}, {}]",
                    challenge.rounds(),
                    low.get(),
                    high.get(),
                    step.get(),
                    if a == b { "agree" } else { "differ" },
                    new_low.get(),
                    new_high.get(),
                );
            }
            Round::Settled { divergence } => {
                println!(
                    "\n    Settled: they agree at {} and disagree at {}. The instruction at {}\n\
                     \x20   is what first made them differ.",
                    divergence.get(),
                    divergence.get() + 1,
                    divergence
                );

                // The last exchange: the state entering the disputed instruction, and what each
                // party claims it became.
                let after = Step::new(divergence.get() + 1);
                return Verdict {
                    divergence,
                    agreed_root: first.root_at(divergence).expect("present"),
                    first_claim: first.root_at(after).expect("present"),
                    second_claim: second.root_at(after).expect("present"),
                    rounds: challenge.rounds(),
                };
            }
        }
    }
}

fn describe(judgment: &Judgment) -> String {
    match judgment {
        Judgment::Guilty { liar } => {
            format!("the {liar} was wrong — its claim does not match what the instruction does")
        }
        Judgment::BothWrong { actual } => format!(
            "neither claim matches; the state actually became {}",
            opt_short(*actual)
        ),
        Judgment::Inconsistent => {
            "inconsistent — both claims match, so the bisection was wrong".to_owned()
        }
    }
}

fn plural(n: usize, noun: &str) -> String {
    format!("{n} {noun}{}", if n == 1 { "" } else { "s" })
}

fn blake3_of(bytes: &[u8]) -> Hash {
    *blake3::hash(bytes).as_bytes()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Roots are 32 bytes and this is a document, so they are shown head and tail.
fn short(hash: &Hash) -> String {
    let full = hex(hash);
    format!("{}…{}", &full[..8], &full[full.len() - 8..])
}

fn opt_short(hash: Option<Hash>) -> String {
    hash.as_ref()
        .map_or_else(|| "(execution had ended)".to_owned(), short)
}

fn rule(title: &str) {
    println!("\n{}", "═".repeat(78));
    println!("  {title}");
    println!("{}\n", "═".repeat(78));
}

fn step_heading(n: u32, title: &str) {
    println!("\n{n}. {title}");
    println!("{}", "─".repeat(78));
}
