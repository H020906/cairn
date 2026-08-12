//! The logarithms.
//!
//! All four share one idea. Write `x = 2^k * (1 + f)` with `1 + f` pinned to
//! `[sqrt(2)/2, sqrt(2)]` — which costs nothing, since `k` is just the exponent field — and then
//! `log(x) = k*ln(2) + log(1 + f)` with `|f| <= 0.4143`. The second term is where the work is,
//! and it is computed from `s = f/(2 + f)` because `log(1+f) = 2*atanh(s)` has a series in odd
//! powers of `s` only. Halving the number of terms halves the rounding error along with it.
//!
//! [`log2`] and [`log10`] are not `ln(x) * constant`. That formulation is off by several units
//! in the last place near `x = 1`, where `ln(x)` is tiny and its own relative error gets
//! multiplied straight into an answer that should have been exact. Both keep the reduced part
//! and the `k*log(2)` part separate to the end, and add them in double precision.

use crate::{LN2_HI, LN2_LO};

/// Minimax coefficients for `log(1+f)` in terms of `s = f/(2+f)`, from fdlibm. The
/// approximation is good to `2^-58.45` on the reduced interval.
const LG1: f64 = 6.666_666_666_666_735_13e-1;
const LG2: f64 = 3.999_999_999_940_941_908e-1;
const LG3: f64 = 2.857_142_874_366_239_149e-1;
const LG4: f64 = 2.222_219_843_214_978_396e-1;
const LG5: f64 = 1.818_357_216_161_805_012e-1;
const LG6: f64 = 1.531_383_769_920_937_332e-1;
const LG7: f64 = 1.479_819_860_511_658_591e-1;

/// The odd-power part of `log(1+f)`, shared by all four functions.
///
/// Returns `s*(hfsq + R)`, which is `log(1+f) - f + f²/2` — the remainder after the two terms
/// the caller can compute more accurately itself. Splitting it this way is what lets [`log2`]
/// and [`log10`] hold `f - f²/2` in extra precision while the polynomial tail rides along at
/// ordinary precision, where it is small enough not to matter.
fn log1p_kernel(f: f64) -> f64 {
    let s = f / (2.0 + f);
    let z = s * s;
    let w = z * z;
    let t1 = w * (LG2 + w * (LG4 + w * LG6));
    let t2 = z * (LG1 + w * (LG3 + w * (LG5 + w * LG7)));
    s * (0.5 * f * f + (t2 + t1))
}

/// Splits `x` as `2^k * (1 + f)` with `1 + f` in `[sqrt(2)/2, sqrt(2)]`.
///
/// Returns `(1 + f, k)`. Assumes `x` is finite, positive and normal — every caller filters
/// those cases first, because each wants to answer them differently.
fn reduce(x: f64) -> (f64, i32) {
    let bits = x.to_bits();
    // Adding the offset moves the sqrt(2) boundary onto a carry out of the mantissa, so the
    // exponent adjustment and the mantissa mask fall out of the same addition.
    let hx = ((bits >> 32) as u32).wrapping_add(0x3ff0_0000 - 0x3fe6_a09e);
    let k = (hx >> 20) as i32 - 0x3ff;
    let hx = (hx & 0x000f_ffff) + 0x3fe6_a09e;
    (
        f64::from_bits(((hx as u64) << 32) | (bits & 0xffff_ffff)),
        k,
    )
}

/// The natural logarithm of `x`.
pub fn ln(mut x: f64) -> f64 {
    // 2^54, for lifting a subnormal into the normal range before reducing it.
    let scale = f64::from_bits(0x4350_0000_0000_0000);
    let bits = x.to_bits();
    let hx = (bits >> 32) as u32;
    let mut k = 0;

    if hx < 0x0010_0000 || hx >> 31 != 0 {
        if bits << 1 == 0 {
            // log(±0) = -inf, reached by dividing rather than named, so it raises the
            // divide-by-zero the standard calls for.
            return -1.0 / (x * x);
        }
        if hx >> 31 != 0 {
            return crate::invalid(x);
        }
        // Subnormal: lift into the normal range so the exponent field means something, and
        // pay for it in `k`.
        k -= 54;
        x *= scale;
    } else if hx >= 0x7ff0_0000 {
        return x;
    } else if hx == 0x3ff0_0000 && bits << 32 == 0 {
        return 0.0;
    }

    let (u, dk) = reduce(x);
    k += dk;
    let f = u - 1.0;
    let hfsq = 0.5 * f * f;
    let r = log1p_kernel(f);
    let dk = k as f64;
    // Ordered so that the two largest terms, `f` and `dk*LN2_HI`, are added last.
    r + dk * LN2_LO - hfsq + f + dk * LN2_HI
}

/// `ln(1 + x)`, computed without the cancellation that spelling it that way would cause.
///
/// For small `x` the sum `1 + x` throws away every bit that carried the answer: at
/// `x = 1e-17`, `(1.0 + x).ln()` is exactly zero while the true value is `1e-17`. This is the
/// mirror of [`expm1`](crate::expm1) and exists for the same reason.
pub fn ln_1p(x: f64) -> f64 {
    let bits = x.to_bits();
    let hu = (bits >> 32) as u32;
    let (f, k, c);

    if hu < 0x3fda_827a || hu >> 31 != 0 {
        // 1 + x < sqrt(2), or x is negative.
        if hu >= 0xbff0_0000 {
            // x <= -1.
            if x.to_bits() == (-1.0_f64).to_bits() {
                return x / 0.0;
            }
            return crate::invalid(x);
        }
        if hu << 1 < 0x3ca0_0000 << 1 {
            // |x| < 2^-53: ln(1+x) and x agree to every bit.
            return x;
        }
        if hu <= 0xbfd2_bec4 {
            // Already inside [sqrt(2)/2, sqrt(2)] once 1 is added, so no reduction is needed
            // and — the point of this branch — no `1 + x` is formed at all.
            (f, k, c) = (x, 0, 0.0);
            return finish(f, k, c);
        }
    } else if hu >= 0x7ff0_0000 {
        return x;
    }

    let u = 1.0 + x;
    let (reduced, kk) = reduce(u);
    // What `1 + x` rounded away. It is the difference between the logarithm actually wanted
    // and the logarithm of the rounded sum, and at k >= 2 the subtraction has to be arranged
    // the other way round to avoid cancelling.
    let cc = if kk < 54 {
        (if kk >= 2 {
            1.0 - (u - x)
        } else {
            x - (u - 1.0)
        }) / u
    } else {
        0.0
    };
    (f, k, c) = (reduced - 1.0, kk, cc);
    finish(f, k, c)
}

/// The tail shared by both of [`ln_1p`]'s paths.
fn finish(f: f64, k: i32, c: f64) -> f64 {
    let hfsq = 0.5 * f * f;
    let r = log1p_kernel(f);
    let dk = k as f64;
    r + (dk * LN2_LO + c) - hfsq + f + dk * LN2_HI
}

/// Splits `x` for [`log2`] and [`log10`], which need the reduced value and the exponent
/// separately and handle the degenerate cases identically.
///
/// Returns `Err` carrying the finished answer when there is no logarithm to compute.
fn split_or_answer(mut x: f64) -> Result<(f64, i32), f64> {
    let scale = f64::from_bits(0x4350_0000_0000_0000);
    let mut bits = x.to_bits();
    let mut hx = (bits >> 32) as u32;
    let mut k = 0;

    // A negative argument reaches this branch too: its high word has the sign bit set, so it
    // is not below the smallest normal when the two are compared as the unsigned patterns
    // they are. Testing the sign separately is what keeps `log2(-1.0)` a NaN.
    if hx < 0x0010_0000 || hx >> 31 != 0 {
        if bits << 1 == 0 {
            return Err(-scale / 0.0);
        }
        if hx >> 31 != 0 {
            return Err(crate::invalid(x));
        }
        k -= 54;
        x *= scale;
        bits = x.to_bits();
        hx = (bits >> 32) as u32;
    }
    if hx >= 0x7ff0_0000 {
        return Err(x + x);
    }
    if hx == 0x3ff0_0000 && bits << 32 == 0 {
        // Exactly 1, and the answer is exactly 0 rather than something a hair either side.
        return Err(0.0);
    }

    k += (hx >> 20) as i32 - 1023;
    let hx = hx & 0x000f_ffff;
    // Normalize to x or x/2, whichever lands inside [sqrt(2)/2, sqrt(2)].
    let i = (hx + 0x95f64) & 0x0010_0000;
    let u = f64::from_bits((((hx | (i ^ 0x3ff0_0000)) as u64) << 32) | (bits & 0xffff_ffff));
    Ok((u, k + (i >> 20) as i32))
}

/// Zeroes the low 32 bits of `x`, leaving a value with at most 21 significant bits.
///
/// Used to split a quantity into a head that multiplies exactly against a 33-bit constant and a
/// tail carrying everything else.
fn head(x: f64) -> f64 {
    f64::from_bits(x.to_bits() & 0xffff_ffff_0000_0000)
}

/// `1 / ln(2)`, split into a head with its low bits clear and the remaining tail.
const INV_LN2_HI: f64 = 1.442_695_040_721_446_275_71e0;
/// The tail of [`INV_LN2_HI`].
const INV_LN2_LO: f64 = 1.675_171_316_488_651_183_53e-10;

/// The base-2 logarithm of `x`.
///
/// Exact on exact powers of two: `log2(1024.0)` is `10.0`, not a neighbour of it.
pub fn log2(x: f64) -> f64 {
    let (u, k) = match split_or_answer(x) {
        Ok(pair) => pair,
        Err(answer) => return answer,
    };
    let y = k as f64;
    let f = u - 1.0;
    let hfsq = 0.5 * f * f;
    let r = log1p_kernel(f);

    // `f - hfsq` carries the whole answer when x is near 1, and multiplying it by a rounded
    // 1/ln(2) would spend several bits there. Splitting both factors keeps them.
    let hi = head(f - hfsq);
    let lo = (f - hi) - hfsq + r;
    let val_hi = hi * INV_LN2_HI;
    let val_lo = (lo + hi) * INV_LN2_LO + lo * INV_LN2_HI;

    // Add the integer part last, in two pieces, so that a large k cannot swamp the fraction.
    let w = y + val_hi;
    (val_lo + ((y - w) + val_hi)) + w
}

/// `1 / ln(10)`, split as [`INV_LN2_HI`] is.
const INV_LN10_HI: f64 = 4.342_944_818_781_688_809_39e-1;
/// The tail of [`INV_LN10_HI`].
const INV_LN10_LO: f64 = 2.508_294_671_164_527_522_98e-11;
/// `log10(2)`, split the same way.
const LOG10_2_HI: f64 = 3.010_299_956_636_117_713_06e-1;
/// The tail of [`LOG10_2_HI`].
const LOG10_2_LO: f64 = 3.694_239_077_158_930_786_16e-13;

/// The base-10 logarithm of `x`.
///
/// Exact on exact powers of ten up to `1e22`, which is as far as `f64` represents them exactly.
pub fn log10(x: f64) -> f64 {
    let (u, k) = match split_or_answer(x) {
        Ok(pair) => pair,
        Err(answer) => return answer,
    };
    let y = k as f64;
    let f = u - 1.0;
    let hfsq = 0.5 * f * f;
    let r = log1p_kernel(f);

    let hi = head(f - hfsq);
    let lo = (f - hi) - hfsq + r;
    let val_hi = hi * INV_LN10_HI;
    let val_lo = y * LOG10_2_LO + (lo + hi) * INV_LN10_LO + lo * INV_LN10_HI;

    let w = y * LOG10_2_HI + val_hi;
    (val_lo + ((y * LOG10_2_HI - w) + val_hi)) + w
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

    #[test]
    fn ln_hits_the_values_that_have_names() {
        assert_eq!(ln(1.0), 0.0);
        assert_eq!(ln(std::f64::consts::E).to_bits(), 1.0_f64.to_bits());
        assert_eq!(ln(0.0), f64::NEG_INFINITY);
        assert_eq!(ln(-0.0), f64::NEG_INFINITY);
        assert!(ln(-1.0).is_nan());
        assert!(ln(f64::NAN).is_nan());
        assert_eq!(ln(f64::INFINITY), f64::INFINITY);
        // A subnormal, which has to be scaled into range before it can be reduced.
        assert!((ln(f64::from_bits(1)) - (-744.440_071_921_381_3)).abs() < 1e-12);
    }

    /// The property [`log2`] and [`log10`] are written the long way for.
    #[test]
    fn the_other_bases_are_exact_on_their_own_powers() {
        for k in -1022..=1023 {
            let x = f64::from_bits(((0x3ff + k) as u64) << 52);
            assert_eq!(log2(x), k as f64, "log2(2^{k})");
        }
        // f64 represents 10^k exactly up to k = 22; past that the input itself is approximate
        // and there is nothing exact left to ask for.
        let mut p = 1.0_f64;
        for k in 0..=22 {
            assert_eq!(log10(p), k as f64, "log10(10^{k})");
            p *= 10.0;
        }
        assert_eq!(log2(1.0), 0.0);
        assert_eq!(log10(1.0), 0.0);
        assert_eq!(log2(0.0), f64::NEG_INFINITY);
        assert_eq!(log10(0.0), f64::NEG_INFINITY);
        assert!(log2(-1.0).is_nan());
        assert!(log10(-1.0).is_nan());
        assert_eq!(log2(f64::INFINITY), f64::INFINITY);
    }

    /// The point of the function: `(1.0 + x).ln()` is zero here, and the answer is not.
    #[test]
    fn ln_1p_keeps_the_answer_where_adding_one_would_destroy_it() {
        let x = 1e-17_f64;
        assert_eq!((1.0 + x).ln(), 0.0);
        assert_eq!(ln_1p(x), x);

        assert_eq!(ln_1p(0.0), 0.0);
        assert!(ln_1p(-0.0).is_sign_negative());
        assert_eq!(ln_1p(-1.0), f64::NEG_INFINITY);
        assert!(ln_1p(-1.5).is_nan());
        assert_eq!(ln_1p(f64::INFINITY), f64::INFINITY);
        assert!(ln_1p(f64::NAN).is_nan());
        // The branch that skips forming `1 + x` at all still has to agree with the branch
        // that does not, to within what each of them carries.
        assert!((ln_1p(-0.2) - ln(0.8)).abs() <= 2.0 * f64::EPSILON * ln(0.8).abs());
        assert!((ln_1p(3.0) - ln(4.0)).abs() <= 2.0 * f64::EPSILON * ln(4.0).abs());
    }
}
