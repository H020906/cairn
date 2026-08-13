//! A Lomb–Scargle periodogram over one frequency band. **The first workload here that is science
//! rather than a fixture.**
//!
//! # What it computes, and why this one
//!
//! Given a series of observations `(t, y)` taken at *uneven* intervals — which is what a telescope
//! produces, because of daylight, weather and scheduling — the Lomb–Scargle periodogram estimates
//! how much power the series contains at each of a set of frequencies. A peak is a candidate
//! period: a variable star, a binary, a transiting planet, a pulsar.
//!
//! It is the standard tool for the job (Lomb 1976, Scargle 1982) and it is what volunteer
//! computing is actually used for — the search that Einstein@Home runs is this shape: scan a band
//! of frequencies, report the peak, move on.
//!
//! **Three properties make it the right choice for Cairn**, and they were the criteria rather
//! than an afterthought:
//!
//! 1. **A unit is a frequency band, and bands are independent.** No shared state, no
//!    communication, no ordering between units. That is the shape of work this whole grid is
//!    built around, and it is not something you can retrofit onto a kernel that lacks it.
//! 2. **It is almost entirely `sin` and `cos`.** WebAssembly has neither, so before
//!    [ADR-0016](../../docs/adr/0016-math-belongs-in-the-module-not-the-host.md) this workload
//!    could not have existed without importing them from the host — and the measurement there is
//!    that V8 and the platform libm disagree on every function tried. A kernel calling `sin` a
//!    million times per unit would have manufactured disputes at roughly that rate, and convicted
//!    whichever honest volunteer was on the losing engine.
//! 3. **The answer can be checked against something other than itself.** Synthesise a signal with
//!    a known period, scan a band containing it, and the peak has to come back at that period.
//!    That is a real acceptance test, not a comparison against a previously recorded output.
//!
//! # The formula
//!
//! For angular frequency `ω`, with a time offset `τ` chosen to make the estimator invariant to
//! shifts of the time origin:
//!
//! ```text
//! tan(2ωτ) = Σ sin(2ω tᵢ) / Σ cos(2ω tᵢ)
//!
//!            1  ⎡ (Σ rᵢ cos ω(tᵢ−τ))²   (Σ rᵢ sin ω(tᵢ−τ))² ⎤
//! P(ω)  =  ───  ⎢ ─────────────────── + ─────────────────── ⎥      rᵢ = yᵢ − ȳ
//!           2σ² ⎣  Σ cos² ω(tᵢ−τ)        Σ sin² ω(tᵢ−τ)      ⎦
//! ```
//!
//! # Determinism, which is the whole point of running it here
//!
//! Floating-point addition is not associative, so **the order of every sum below is part of the
//! answer.** They are written as sequential loops over the input in the order it arrived, and that
//! is deliberate: no parallel reduction, no reassociation, no compensated summation that varies
//! with the compiler's mood. Rust does not enable fast-math, so the emitted WebAssembly performs
//! exactly these operations in exactly this order on every engine.
//!
//! Two consequences worth stating, because both are the kind of thing that looks like a bug later:
//!
//! - **A more accurate summation would be a different answer**, not a better one. Kahan summation
//!   here would change every result, and it would be fine — but only if every volunteer changed
//!   at the same instant, which is what the unit id being a hash of the module bytes enforces.
//! - **The result is not the mathematically exact periodogram** and does not claim to be. It is a
//!   specific, reproducible sequence of `f64` operations that approximates it. Cairn verifies that
//!   two volunteers computed *the same thing*, and it has nothing to say about whether that thing
//!   is the right science. That remains the workload author's problem.
//!
//! # Input and output
//!
//! Little-endian throughout, because that is what WebAssembly's loads and stores are.
//!
//! ```text
//! input   f64  lowest frequency in the band
//!         f64  highest frequency in the band
//!         u32  how many frequencies to sample across it
//!         u32  how many observations follow
//!         (f64 time, f64 value) × that many
//!
//! output  f64  the frequency with the most power
//!         f64  that power
//!         f64  the sum of the power at every frequency scanned
//! ```
//!
//! **The third output field is there for verification rather than for science.** The peak alone
//! would hide a disagreement anywhere else in the band: two volunteers could differ on nine
//! hundred frequencies and agree on the peak, and the grid would accept it. The sum is affected by
//! every frequency scanned, so it turns the whole band into something two parties must agree
//! about. It costs one addition per frequency.

/// How many bytes of observations this unit will accept.
///
/// A stack buffer, so it lives inside the shadow stack that `.cargo/config.toml` sizes — 128 KiB
/// there against 32 KiB here, which leaves room for frames. Sixteen bytes per observation makes
/// this a little over two thousand of them, and a band of a few thousand frequencies over that
/// many points is a unit of a few hundred milliseconds. That is the size a work unit wants to be:
/// long enough that the round trip is not the cost, short enough that a volunteer closing a laptop
/// loses little.
const INPUT: usize = 32 * 1024;

/// Three `f64`s.
const OUTPUT: usize = 24;

/// Bytes of header before the observations begin.
const HEADER: usize = 24;

/// Bytes per observation.
const OBSERVATION: usize = 16;

/// The most observations that fit, which is what bounds the loops below.
const MAX_OBSERVATIONS: usize = (INPUT - HEADER) / OBSERVATION;

cairn_workload::workload! {
    input: INPUT,
    output: OUTPUT,
    run: run,
}

/// Read a little-endian `f64` at `offset`, or trap.
///
/// Trapping on a short input rather than defaulting is the same judgement the SDK makes about a
/// truncated answer: a unit that quietly computed on zeros would return a number, and a number is
/// worse than a trap here — it would be accepted, or it would put an honest volunteer into a
/// dispute over an answer neither party meant to compute.
fn f64_at(bytes: &[u8], offset: usize) -> f64 {
    let Some(slice) = bytes.get(offset..offset + 8) else {
        cairn_workload::trap()
    };
    let mut word = [0u8; 8];
    word.copy_from_slice(slice);
    f64::from_le_bytes(word)
}

/// Read a little-endian `u32` at `offset`, or trap.
fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    let Some(slice) = bytes.get(offset..offset + 4) else {
        cairn_workload::trap()
    };
    let mut word = [0u8; 4];
    word.copy_from_slice(slice);
    u32::from_le_bytes(word)
}

fn run(input: &[u8], answer: &mut cairn_workload::Answer<'_>) {
    let frequency_low = f64_at(input, 0);
    let frequency_high = f64_at(input, 8);
    let frequency_count = u32_at(input, 16) as usize;
    let observation_count = u32_at(input, 20) as usize;

    // Two observations is the minimum that has a variance at all, and a band has to contain at
    // least one frequency to be a band. Anything else is a malformed unit rather than a hard
    // problem, and a trap says so in the one way every engine agrees about.
    if observation_count < 2 || observation_count > MAX_OBSERVATIONS || frequency_count == 0 {
        cairn_workload::trap();
    }

    let mut times = [0.0f64; MAX_OBSERVATIONS];
    let mut residuals = [0.0f64; MAX_OBSERVATIONS];

    let mut total = 0.0f64;
    for index in 0..observation_count {
        let at = HEADER + index * OBSERVATION;
        times[index] = f64_at(input, at);
        residuals[index] = f64_at(input, at + 8);
        total += residuals[index];
    }

    // The mean is subtracted once, here, rather than inside the frequency loop. That is an
    // arithmetic decision as much as a speed one: subtracting it per frequency would be a
    // different sequence of roundings and therefore a different answer.
    let count = observation_count as f64;
    let mean = total / count;
    let mut variance = 0.0f64;
    for value in residuals.iter_mut().take(observation_count) {
        *value -= mean;
        variance += *value * *value;
    }
    variance /= count - 1.0;

    // A constant series has no variance and every frequency has the same nothing in it. Dividing
    // by zero would give infinities that compare and sort perfectly consistently — this is not
    // about avoiding a NaN, it is that "the first frequency, with infinite power" is a worse
    // answer to return than "no frequency, with none".
    if !(variance > 0.0) {
        answer.push(&frequency_low.to_le_bytes());
        answer.push(&0.0f64.to_le_bytes());
        answer.push(&0.0f64.to_le_bytes());
        return;
    }

    let times = &times[..observation_count];
    let residuals = &residuals[..observation_count];

    let mut best_frequency = frequency_low;
    let mut best_power = f64::NEG_INFINITY;
    let mut summed_power = 0.0f64;

    // The band is divided into `frequency_count` samples with both endpoints included, so two
    // adjacent bands handed to two volunteers overlap at exactly one frequency and neither leaves
    // a gap. A step computed once and added repeatedly would drift; this recomputes from the ends
    // every time, so the frequency a unit reports depends only on its band and its index.
    let span = frequency_high - frequency_low;
    let divisor = if frequency_count > 1 {
        (frequency_count - 1) as f64
    } else {
        1.0
    };

    for index in 0..frequency_count {
        let frequency = frequency_low + span * (index as f64) / divisor;
        let power = power_at(times, residuals, variance, frequency);
        summed_power += power;
        if power > best_power {
            best_power = power;
            best_frequency = frequency;
        }
    }

    answer.push(&best_frequency.to_le_bytes());
    answer.push(&best_power.to_le_bytes());
    answer.push(&summed_power.to_le_bytes());
}

/// Lomb–Scargle power at one frequency.
///
/// Two passes over the observations: the first finds `τ`, the second accumulates the four sums the
/// estimator needs. Both are sequential and in input order, which is what makes this the same
/// answer everywhere.
fn power_at(times: &[f64], residuals: &[f64], variance: f64, frequency: f64) -> f64 {
    let omega = core::f64::consts::TAU * frequency;
    let double = 2.0 * omega;

    let mut sin_sum = 0.0f64;
    let mut cos_sum = 0.0f64;
    for time in times {
        sin_sum += cairn_math::sin(double * time);
        cos_sum += cairn_math::cos(double * time);
    }

    // `atan2` rather than `atan` of a ratio, so that `cos_sum == 0` is an ordinary case rather
    // than a division by zero, and so the quadrant is right. `atan2(0, 0)` is 0 by the standard,
    // which is as good an answer as any when there is no phase to find.
    let tau = if double == 0.0 {
        0.0
    } else {
        cairn_math::atan2(sin_sum, cos_sum) / double
    };

    let mut cross_cos = 0.0f64;
    let mut cross_sin = 0.0f64;
    let mut cos_squared = 0.0f64;
    let mut sin_squared = 0.0f64;
    for (time, residual) in times.iter().zip(residuals) {
        let angle = omega * (time - tau);
        let cosine = cairn_math::cos(angle);
        let sine = cairn_math::sin(angle);
        cross_cos += residual * cosine;
        cross_sin += residual * sine;
        cos_squared += cosine * cosine;
        sin_squared += sine * sine;
    }

    // At zero frequency every sine is zero and `sin_squared` with it. Contributing nothing is
    // correct there — the constant term was already removed with the mean — and it keeps the band
    // scannable from zero without a special case at the call site.
    let cosine_term = if cos_squared > 0.0 {
        cross_cos * cross_cos / cos_squared
    } else {
        0.0
    };
    let sine_term = if sin_squared > 0.0 {
        cross_sin * cross_sin / sin_squared
    } else {
        0.0
    };

    0.5 * (cosine_term + sine_term) / variance
}
