//! Transcendental math that computes the same bits on every engine.
//!
//! # The measurement this crate exists because of
//!
//! WebAssembly has no `exp`, no `log`, no `sin`. A workload that needs them has two places to
//! get them: **from the host**, as an import, or **from inside its own module**, compiled in.
//! The obvious choice is the host — it is one import instead of a kilobyte of polynomial, and
//! every host already has a `Math.exp`.
//!
//! It is also the choice that would break Cairn. Twenty thousand inputs, twelve functions,
//! V8 against the platform libm this repository's tests run on:
//!
//! | function | inputs where the two hosts disagree | | function | disagree |
//! |---|---|---|---|---|
//! | `cbrt` | **29.80%** | | `exp` | 7.41% |
//! | `sinh` | **17.82%** | | `tan` | 3.63% |
//! | `tanh` | **13.77%** | | `ln` | 3.52% |
//! | `log10` | **8.98%** | | `cos` | 2.54% |
//! | `sin` | 2.17% | | `asin` | 1.87% |
//! | `atan` | 0.32% | | `pow` | 0.01% |
//!
//! Twelve functions, twelve disagreements, none of them rare. The gaps are one or two units in
//! the last place — which in ordinary numerical work is nothing, and here is everything. Cairn
//! settles a disagreement by finding the first instruction at which two workers diverged and
//! ruling against one of them. It has no notion of *nearly the same*. A browser volunteer and a
//! native volunteer computing `cbrt` would take opposite sides of a dispute on close to one
//! call in three, and arbitration would convict whichever of the two honest volunteers happened
//! to be running the engine that lost.
//!
//! So: **math never comes from the host.** `validate` already enforces the mechanism — the only
//! importable module is `cairn`, and it has exactly three functions, none of them arithmetic —
//! and [ADR-0016](../../../../docs/adr/0016-math-belongs-in-the-module-not-the-host.md) records
//! the reasoning. This crate is what makes that rule livable, by giving a workload author the
//! functions they were going to reach for anyway.
//!
//! # Why these bits are the same everywhere
//!
//! Every operation used below is one whose result WebAssembly specifies exactly, for every
//! input, on every engine:
//!
//! - `+`, `-`, `*`, `/` — IEEE-754 round-to-nearest-even, mandated.
//! - `sqrt` — likewise. IEEE-754 requires it be correctly rounded, and Wasm inherits that.
//! - `floor`, `ceil`, `trunc` — exact by definition.
//! - `abs`, `copysign`, `to_bits`, `from_bits` — bit manipulation.
//! - Integer arithmetic, including the 128-bit multiplies in [`trig`]'s argument reduction.
//!
//! There is nothing else. No `f64::exp`, no `mul_add` — the latter compiles to a call to the
//! platform's `fma` on targets without the instruction, which puts a libm back in the module
//! through the side door. The rule is checked, not merely stated: `no_host_math_reaches_the_module`
//! in `tests/wasm.rs` compiles this crate to WebAssembly and asserts that the resulting module
//! imports **exactly `cairn.input` and `cairn.output`, and nothing else**.
//!
//! # The other half of the argument, which turned up by accident
//!
//! Host math is not only inconsistent between hosts. It is not dependably correct on any one of
//! them. For `x = 6381956970095103 * 2^797` — the worst case for argument reduction in this
//! format — the true value of `sin(x)` is `1.0`, confirmed by exact integer arithmetic over a
//! 3000-bit `pi`, by V8, and by this crate, all three agreeing to the last bit. **The platform
//! libm these tests run against returns `-0.2227`**: not a rounding difference, a wrong answer,
//! from a shipping library, with nothing to indicate anything went wrong. See `trig`'s
//! `the_worst_case_in_the_format_comes_out_right_even_though_the_platform_gets_it_wrong`.
//!
//! ## What is taken from `std`, and why that is not a hole
//!
//! This crate is not `no_std`, and it needs exactly four things that `core` does not have:
//! `sqrt`, `floor`, `ceil` and `trunc`. Each is a *single WebAssembly instruction* —
//! `f64.sqrt`, `f64.floor`, `f64.ceil`, `f64.trunc` — with a result the specification pins
//! down completely. They are in the list above precisely because they are safe. Reimplementing
//! them here would be slower and no more deterministic.
//!
//! The transcendental functions are a different kind of thing. IEEE-754 does not require any
//! particular result for `exp`, only that it be a decent approximation, and that freedom is
//! exactly what the table above measures. Those are the ones this crate has to own.
//!
//! # Accuracy, and what it is measured against
//!
//! The algorithms are the classic ones from Sun Microsystems' **fdlibm** (1993), the ancestor
//! of the math library in musl, in Go, in Java's `StrictMath`, and in most other places. They
//! were not chosen for novelty. They are the most heavily exercised numeric kernels in
//! existence, and the point of this crate is bit-reproducibility rather than a better `exp`.
//!
//! `tests/accuracy.rs` compares every function against the platform libm over a large sample
//! and reports the worst error in units in the last place. Those numbers are printed by the
//! test rather than asserted loosely, so a regression shows up as a number that moved:
//!
//! ```text
//! cargo test -p cairn-math --release -- --nocapture accuracy
//! ```
//!
//! # Determinism is tested, not assumed
//!
//! Accuracy against the host libm is a *quality* measurement. It is not the property Cairn
//! needs. The property Cairn needs is that four independent engines produce identical bits, and
//! that is checked where every other such claim in this project is checked — in
//! `runtime/tests/differential.rs`, which runs a compiled `cairn-math` corpus through Cairn's
//! own interpreter, wasmi, wasmtime, and the V8 in the volunteer's browser.

#![forbid(unsafe_code)]
// **This crate is not `no_std`, and it cannot be — which is worth writing down, because it looks
// like it should be.** Every line of it is arithmetic over `f64` with no allocation and no host
// underneath, so `#![no_std]` is the obvious shape. It fails on five methods:
//
//     f64::sqrt   f64::floor   f64::ceil   f64::trunc   f64::round_ties_even
//
// All five are **single WebAssembly instructions** — `f64.sqrt`, `f64.floor`, `f64.ceil`,
// `f64.trunc`, `f64.nearest` — and all five live in `std` rather than `core`, because on an
// ordinary target they are libm calls. On `wasm32-unknown-unknown` they compile to the
// instruction and import nothing, which `tests/wasm.rs` checks by reading the module's import
// section: it is exactly `cairn.input` and `cairn.output`.
//
// **So `no_std` would be a proxy for the property Cairn needs, and the property is directly
// checkable.** What matters is that no math comes from the host, and the import list says whether
// it does. Reaching for `no_std` here would mean writing a software `sqrt` in place of an
// instruction the specification defines exactly — trading a checked property for an unchecked one
// and a correctly-rounded result for a hand-rolled one. See ADR-0019.
//
// The polynomial coefficients below are written with more decimal digits than a `f64` holds.
// That is deliberate and worth the lint: they are transcribed from fdlibm exactly as published,
// so a reader can compare them character by character against the reference rather than
// trusting that a truncation rounded back to the same value. The stored `f64` is identical
// either way — truncating would change nothing but the ability to check the transcription.
#![allow(clippy::excessive_precision)]

mod exp;
mod inverse;
mod log;
mod pow;
mod roots;
mod trig;

pub use exp::{cosh, exp, exp2, expm1, sinh, tanh};
pub use inverse::{acos, asin, atan, atan2};
pub use log::{ln, ln_1p, log10, log2};
pub use pow::pow;
pub use roots::{cbrt, hypot, sqrt};
pub use trig::{cos, sin, tan};

/// `ln(2)`, split so that `LN2_HI` has its low 20 bits clear.
///
/// The split is what makes `k * LN2_HI` exact for the small integer `k` that argument
/// reduction produces: `k` has at most 11 significant bits and `LN2_HI` has 33, so the product
/// fits in a `f64` with room to spare. All the discarded precision lives in `LN2_LO`, which is
/// applied separately and at a magnitude where its own rounding cannot matter.
pub(crate) const LN2_HI: f64 = 6.931_471_803_691_238_164_9e-1;
/// The tail of `ln(2)` left over from [`LN2_HI`].
pub(crate) const LN2_LO: f64 = 1.908_214_929_270_587_700_02e-10;
/// `1 / ln(2)`, used to choose how many powers of two to pull out of an exponential.
pub(crate) const INV_LN2: f64 = core::f64::consts::LOG2_E;
/// `ln(2)` to full `f64` precision, for the places that do not need the split form.
pub(crate) const LN2: f64 = core::f64::consts::LN_2;

/// Multiplies `x` by two raised to `n`, without overflowing on the way there.
///
/// The obvious implementation — build `2^n` and multiply — is wrong at the ends: `2^n` is not
/// representable for `|n| > 1023`, so a perfectly finite answer like `scalbn(2^-1000, 1500)`
/// would arrive through an infinity. Splitting the scaling into at most three steps keeps every
/// intermediate in range.
pub(crate) fn scalbn(x: f64, mut n: i32) -> f64 {
    // 2^1023 and 2^-1022, as bit patterns, so that no decimal literal has to round correctly.
    let huge = f64::from_bits(0x7fe0_0000_0000_0000);
    let tiny = f64::from_bits(0x0010_0000_0000_0000);
    // 2^53, for stepping out of the subnormal range without losing bits to it.
    let step = f64::from_bits(0x4340_0000_0000_0000);

    let mut y = x;
    if n > 1023 {
        y *= huge;
        n -= 1023;
        if n > 1023 {
            y *= huge;
            n -= 1023;
            if n > 1023 {
                n = 1023;
            }
        }
    } else if n < -1022 {
        // Scale by 2^-1022 * 2^53 rather than 2^-1022 alone: multiplying straight down into the
        // subnormals would round away bits that the second step cannot bring back.
        y *= tiny * step;
        n += 1022 - 53;
        if n < -1022 {
            y *= tiny * step;
            n += 1022 - 53;
            if n < -1022 {
                n = -1022;
            }
        }
    }
    y * f64::from_bits(((0x3ff + n) as u64) << 52)
}

/// The largest integer no greater than `x`.
///
/// A single `f64.floor` instruction on WebAssembly.
#[inline]
pub fn floor(x: f64) -> f64 {
    x.floor()
}

/// The smallest integer no less than `x`.
///
/// A single `f64.ceil` instruction on WebAssembly.
#[inline]
pub fn ceil(x: f64) -> f64 {
    x.ceil()
}

/// `x` with any fractional part discarded, rounding toward zero.
///
/// A single `f64.trunc` instruction on WebAssembly.
#[inline]
pub fn trunc(x: f64) -> f64 {
    x.trunc()
}

/// The nearest integer to `x`, with halves going away from zero.
///
/// **This is not `f64.nearest`,** which breaks ties to even: `nearest(2.5)` is `2`, and this is
/// `3`. The distinction is why the function is written out rather than deferred to an
/// instruction — the C `round` a workload author is thinking of is this one.
pub fn round(x: f64) -> f64 {
    let whole = x.trunc();
    // For |x| >= 2^52 there is no fractional part left to examine and `whole == x`, so the
    // comparison below is false and the value is returned unchanged, which is correct.
    if (x - whole).abs() >= 0.5 {
        whole + 1.0_f64.copysign(x)
    } else {
        // `copysign` preserves the sign of a zero result: `round(-0.4)` is `-0.0`, not `0.0`.
        whole.copysign(x)
    }
}

/// The remainder of `x / y` with the sign of `x`, exactly.
///
/// Exactly, and therefore not by any route through division. `x - trunc(x / y) * y` loses the
/// answer entirely once `x / y` exceeds `2^53`, because the quotient it rounds to is no longer
/// the quotient. This works on the mantissas as integers instead, which is slower and always
/// right.
pub fn fmod(x: f64, y: f64) -> f64 {
    let (bx, by) = (x.to_bits(), y.to_bits());
    let (mut ex, mut ey) = (((bx >> 52) & 0x7ff) as i32, ((by >> 52) & 0x7ff) as i32);
    let sign = bx & (1 << 63);

    // y == 0, x infinite, or either a NaN: all of these are a NaN by IEEE-754.
    if by << 1 == 0 || y.is_nan() || ex == 0x7ff {
        return invalid(x * y);
    }
    // |x| <= |y| covers x == 0 too. Equal magnitudes leave a zero carrying x's sign.
    if bx << 1 <= by << 1 {
        return if bx << 1 == by << 1 { 0.0 * x } else { x };
    }

    // Normalize both to a 53-bit integer with the leading bit at position 52, keeping the
    // biased exponent as the scale. A subnormal has no implicit bit, so it is shifted up until
    // it has one and its exponent goes negative to pay for it.
    let normalize = |bits: u64, exp: &mut i32| -> u64 {
        if *exp == 0 {
            let mut probe = bits << 12;
            while probe >> 63 == 0 {
                probe <<= 1;
                *exp -= 1;
            }
            // Shifting by `1 - exp` also carries the sign bit off the top, which is why the
            // sign was taken before any of this.
            bits << (1 - *exp)
        } else {
            (bits & (u64::MAX >> 12)) | (1 << 52)
        }
    };
    let mut mx = normalize(bx, &mut ex);
    let my = normalize(by, &mut ey);

    // Long division, one bit at a time. `mx` starts below `2*my` and the invariant holds
    // through every iteration, so a single conditional subtraction per bit is enough.
    for _ in 0..(ex - ey) {
        let rest = mx.wrapping_sub(my);
        if rest >> 63 == 0 {
            if rest == 0 {
                return 0.0 * x;
            }
            mx = rest;
        }
        mx <<= 1;
    }
    let rest = mx.wrapping_sub(my);
    if rest >> 63 == 0 {
        if rest == 0 {
            return 0.0 * x;
        }
        mx = rest;
    }

    // The remainder is scaled at `ey` now. Shift its leading bit back to position 52, paying
    // for each shift out of the exponent.
    let mut e = ey;
    while mx >> 52 == 0 {
        mx <<= 1;
        e -= 1;
    }
    if e > 0 {
        // Drop the explicit leading bit and write the exponent in its place.
        f64::from_bits((mx - (1 << 52)) | ((e as u64) << 52) | sign)
    } else {
        // The remainder is subnormal, so the leading bit stays and the value is shifted down.
        f64::from_bits((mx >> (1 - e)) | sign)
    }
}

/// A NaN, produced out of `x` rather than named.
///
/// Every function here has arguments it has no answer for — `ln` of a negative, `asin` above
/// one, a fractional power of a negative base. Returning `f64::NAN` would give the right value
/// and raise nothing; this raises the invalid operation the standard asks for, and propagates
/// an argument that was already a NaN.
///
/// Subtracting a value from itself is not a redundant expression here. It is the only way to
/// spell *invalid* in a language with no access to the floating-point environment, and it is
/// what every libm does.
#[inline]
#[allow(clippy::eq_op)]
pub(crate) fn invalid(x: f64) -> f64 {
    (x - x) / (x - x)
}

/// Two doubles whose sum is the exact sum of `a` and `b`.
///
/// Knuth's two-sum. `s` is the rounded sum and `e` is precisely what the rounding threw away,
/// so `a + b == s + e` with no error at all — which is the whole trick behind carrying more
/// than 53 bits of precision through a calculation without a bignum.
#[inline]
pub(crate) fn two_sum(a: f64, b: f64) -> (f64, f64) {
    let s = a + b;
    let bb = s - a;
    (s, (a - (s - bb)) + (b - bb))
}

/// Two doubles whose sum is the exact product of `a` and `b`.
///
/// Dekker's algorithm. Each operand is split into two halves of at most 26 significant bits, so
/// that the four cross products are individually exact, and the error term is then assembled
/// from them. This is the fused multiply-add that WebAssembly does not have, built out of
/// operations it does.
#[inline]
pub(crate) fn two_product(a: f64, b: f64) -> (f64, f64) {
    // 2^27 + 1. Multiplying by this and subtracting is Veltkamp's exact splitting step.
    const SPLIT: f64 = 134_217_729.0;
    let split = |v: f64| {
        let c = SPLIT * v;
        let hi = c - (c - v);
        (hi, v - hi)
    };
    let (ahi, alo) = split(a);
    let (bhi, blo) = split(b);
    let p = a * b;
    (p, ((ahi * bhi - p) + ahi * blo + alo * bhi) + alo * blo)
}

/// Adds a double to a double-double, keeping the extra precision.
#[inline]
pub(crate) fn dd_add(hi: f64, lo: f64, x: f64) -> (f64, f64) {
    let (s, e) = two_sum(hi, x);
    let (s, e2) = two_sum(s, e + lo);
    (s, e2)
}

#[cfg(test)]
/// Exact equality is the claim these tests make, not an accident of writing them: this crate
/// exists to produce particular bits, so `assert_eq!` on a float is the assertion that
/// belongs. The workspace denies `float_cmp` because a comparison must always be
/// deliberate — here every one of them is, and the lint stays on in the code above, where
/// an accidental comparison could still hide.
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    /// The constants are split by hand, so the splitting is checked rather than trusted.
    #[test]
    fn the_split_constants_add_back_up_to_what_they_claim_to_be() {
        // LN2_HI must have its low 21 bits clear, leaving 33 significant ones, or
        // `k * LN2_HI` stops being exact for the `k` argument reduction produces.
        assert_eq!(LN2_HI.to_bits() & 0x1f_ffff, 0);
        // Reassembling the two halves must land on the full-precision value.
        assert_eq!((LN2_HI + LN2_LO).to_bits(), LN2.to_bits());
        // And that value must be ln(2) as well as f64 can say it.
        assert_eq!(LN2.to_bits(), 0x3fe6_2e42_fefa_39ef);
        assert_eq!(INV_LN2.to_bits(), 0x3ff7_1547_652b_82fe);
    }

    #[test]
    fn scalbn_reaches_both_ends_without_going_through_infinity() {
        // The case the three-step scaling exists for: a finite answer, 2^200, whose naive
        // route multiplies by a 2^1200 that is not representable.
        let two_pow = |n: i32| f64::from_bits(((1023 + n) as u64) << 52);
        assert_eq!(scalbn(two_pow(-1000), 1200), two_pow(200));
        // And the other direction, which would go through a zero instead.
        assert_eq!(scalbn(two_pow(1000), -1200), two_pow(-200));
        assert_eq!(scalbn(1.0, 0), 1.0);
        assert_eq!(scalbn(1.0, 5), 32.0);
        assert_eq!(scalbn(1.0, -1074), f64::from_bits(1));
        assert_eq!(scalbn(1.0, 2000), f64::INFINITY);
        assert_eq!(scalbn(1.0, -2000), 0.0);
    }

    #[test]
    fn two_product_recovers_the_bits_multiplication_discards() {
        // (1 + 2^-52)² is exactly 1 + 2^-51 + 2^-104. The first two terms are representable
        // and the third is not, so the answer is known in advance: the rounded product is
        // 1 + 2^-51 and the recovered error is exactly 2^-104.
        let a = f64::from_bits(0x3ff0_0000_0000_0001);
        let (p, e) = two_product(a, a);
        assert_eq!(
            p.to_bits(),
            (1.0 + f64::from_bits(0x3cc0_0000_0000_0000)).to_bits()
        );
        assert_eq!(e.to_bits(), f64::from_bits(0x3970_0000_0000_0000).to_bits());
        // And where the product does fit, there is nothing left over.
        let (p, e) = two_product(3.0, 5.0);
        assert_eq!(p, 15.0);
        assert_eq!(e, 0.0);
    }

    #[test]
    fn round_goes_away_from_zero_where_nearest_would_go_to_even() {
        assert_eq!(round(2.5), 3.0);
        assert_eq!(round(-2.5), -3.0);
        assert_eq!(round(0.5), 1.0);
        assert_eq!(round(1.5), 2.0);
        // The instruction this deliberately is not.
        assert_eq!(2.5_f64.round_ties_even(), 2.0);
        // Sign of a zero result survives.
        assert!(round(-0.4).is_sign_negative());
        assert_eq!(round(-0.4), 0.0);
        // Already-integral values, including ones too large to have a fraction.
        assert_eq!(round(1e300), 1e300);
        assert_eq!(round(f64::INFINITY), f64::INFINITY);
    }

    #[test]
    fn fmod_is_exact_where_the_divide_and_subtract_route_has_no_bits_left() {
        // x / y here is about 2^995, far past where a f64 quotient means anything, and the
        // naive `x - trunc(x/y)*y` returns 0. The right answer is not 0.
        let (x, y) = (1e300_f64, 7.0_f64);
        assert_eq!(fmod(x, y), x % y);
        assert_ne!(fmod(x, y), 0.0);
        // The route not taken, for contrast: it loses the answer completely.
        assert_eq!(x - (x / y).trunc() * y, 0.0);

        assert_eq!(fmod(5.5, 2.0), 1.5);
        assert_eq!(fmod(-5.5, 2.0), -1.5);
        assert_eq!(fmod(5.5, -2.0), 1.5);
        // Exact multiples keep the sign of the dividend.
        assert!(fmod(-4.0, 2.0).is_sign_negative());
        assert_eq!(fmod(-4.0, 2.0), 0.0);
        // |x| < |y| is the identity.
        assert_eq!(fmod(1.0, 3.0), 1.0);
        // Degenerate cases are NaN.
        assert!(fmod(1.0, 0.0).is_nan());
        assert!(fmod(f64::INFINITY, 1.0).is_nan());
        // A subnormal divisor still normalizes.
        let sub = f64::from_bits(0x000f_ffff_ffff_ffff);
        assert_eq!(fmod(1.0, sub), 1.0_f64 % sub);
    }

    /// Agreement with the platform's `%` over a spread of magnitudes, since `fmod` is exact and
    /// so is `%` — this one is allowed to demand equality rather than closeness.
    #[test]
    fn fmod_agrees_with_the_platform_everywhere_it_is_asked() {
        let mut state = 0x243f_6a88_85a3_08d3_u64;
        let mut next = || {
            state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        };
        for _ in 0..200_000 {
            let x = f64::from_bits(next() & 0x7fef_ffff_ffff_ffff | (next() & (1 << 63)));
            let y = f64::from_bits(next() & 0x7fef_ffff_ffff_ffff | (next() & (1 << 63)));
            let (mine, theirs) = (fmod(x, y), x % y);
            assert_eq!(
                mine.to_bits(),
                theirs.to_bits(),
                "fmod({x:e}, {y:e}) gave {mine:e}, platform gave {theirs:e}"
            );
        }
    }
}
