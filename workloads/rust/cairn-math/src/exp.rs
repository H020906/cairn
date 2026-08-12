//! `exp`, its relatives, and the three hyperbolic functions built on them.
//!
//! Everything here rests on one kernel. Reduce the argument to `x = k*ln(2) + r` with
//! `|r| <= ln(2)/2`, approximate `exp(r)` by a rational function on that short interval, and
//! scale by `2^k` — which is exact, because it only touches the exponent. The polynomial is
//! fdlibm's degree-5 minimax fit to `R(z) = r*(exp(r)+1)/(exp(r)-1)`, chosen over a direct fit
//! to `exp(r)` because `R` is even and so buys twice the accuracy per coefficient.
//!
//! The reduction is where the precision is won or lost, and it is why `ln(2)` appears in two
//! pieces. `k*LN2_HI` is *exact* — `k` is small and `LN2_HI` has its low bits cleared — so the
//! subtraction `x - k*LN2_HI` introduces no error of its own even when it cancels almost
//! everything. The rest of `ln(2)` is applied afterwards, at a magnitude where its rounding is
//! far below the answer's last bit.

use crate::{scalbn, two_product, INV_LN2, LN2_HI, LN2_LO};

/// Minimax coefficients for `R(z) = 2 + P1*z + ... + P5*z^5`, from fdlibm, accurate to
/// `2^-59` on `[0, 0.34658]`.
const P1: f64 = 1.666_666_666_666_660_190_37e-1;
const P2: f64 = -2.777_777_777_701_559_338_42e-3;
const P3: f64 = 6.613_756_321_437_934_361_17e-5;
const P4: f64 = -1.653_390_220_546_525_153_90e-6;
const P5: f64 = 4.138_136_797_057_238_460_39e-8;

/// `2^k * exp(hi - lo)`, for `|hi - lo| <= ln(2)/2`.
///
/// The argument arrives already split, because the caller is the only one who knows how it was
/// reduced and therefore where the extra precision is. Passing `hi` and `lo` separately rather
/// than their sum is what keeps that precision: `-lo + hi` is re-added at the end at full
/// width, and it is worth roughly ten bits of the result.
fn kernel(hi: f64, lo: f64, k: i32) -> f64 {
    let x = hi - lo;
    let xx = x * x;
    let c = x - xx * (P1 + xx * (P2 + xx * (P3 + xx * (P4 + xx * P5))));
    // exp(r) = 1 + r + r*c/(2 - c), with the `r` written as `hi - lo` so no precision is lost
    // re-adding it.
    let y = 1.0 + (x * c / (2.0 - c) - lo + hi);
    if k == 0 {
        y
    } else {
        scalbn(y, k)
    }
}

/// `e` raised to the power `x`.
///
/// Accurate to well under one unit in the last place. Overflows to infinity above
/// `709.782712893383973096` and underflows to zero below `-745.13321910194110842`; between
/// the latter and `-708.396` the result is subnormal and correspondingly less precise, which is
/// a property of the format rather than of this code.
pub fn exp(x: f64) -> f64 {
    let huge = f64::from_bits(0x7fe0_0000_0000_0000);
    let bits = x.to_bits();
    let sign = (bits >> 63) as usize;
    let hx = (bits >> 32) as u32 & 0x7fff_ffff;

    if hx >= 0x4086_232b {
        // |x| >= 708.39: at or past both ends of the representable range.
        if x.is_nan() {
            return x;
        }
        if x > 709.782_712_893_383_973_096 {
            // Reached through a multiplication so that an infinite `x` stays infinite and a
            // finite one raises overflow rather than arriving at infinity silently.
            return x * huge;
        }
        if x < -745.133_219_101_941_108_42 {
            return 0.0;
        }
        // Between -745.13 and -708.39 the answer is subnormal but not zero, so fall through.
    }

    let (hi, lo, k);
    if hx > 0x3fd6_2e42 {
        // |x| > ln(2)/2, so there is a power of two to pull out.
        let n = if hx >= 0x3ff0_a2b2 {
            // |x| >= 1.5*ln(2): round x/ln(2) to nearest, biased by the sign so that the
            // truncating cast rounds rather than floors.
            (INV_LN2 * x + if sign == 1 { -0.5 } else { 0.5 }) as i32
        } else {
            // Between 0.5 and 1.5 times ln(2): the answer is +1 or -1 and the multiply is a
            // waste.
            1 - 2 * sign as i32
        };
        hi = x - n as f64 * LN2_HI;
        lo = n as f64 * LN2_LO;
        k = n;
    } else if hx > 0x3e30_0000 {
        // |x| > 2^-28 but no reduction needed.
        (hi, lo, k) = (x, 0.0, 0);
    } else {
        // |x| <= 2^-28: exp(x) and 1+x agree to every bit f64 has.
        return 1.0 + x;
    }

    kernel(hi, lo, k)
}

/// Two raised to the power `x`.
///
/// Exact for integral `x` in range, which is the property most callers actually want from it:
/// `exp2(10.0)` is `1024.0` and not something a hair away from it.
pub fn exp2(x: f64) -> f64 {
    if x.is_nan() {
        return x;
    }
    if x >= 1024.0 {
        return f64::INFINITY;
    }
    if x < -1075.0 {
        return 0.0;
    }

    // Split into an integer part, which becomes an exponent, and |t| <= 0.5.
    let n = x.round_ties_even();
    let t = x - n;
    let k = n as i32;

    // r = t*ln(2), to more than f64 precision. `two_product` returns the part of `t*LN2_HI`
    // that the multiplication rounded off, and `t*LN2_LO` supplies the rest of ln(2); together
    // they are the tail the kernel needs to stay accurate through the cancellation.
    let (p, e) = two_product(t, LN2_HI);
    let tail = t * LN2_LO + e;
    kernel(p, -tail, k)
}

/// Scaled coefficients for the `expm1` rational approximation, from fdlibm.
const Q1: f64 = -3.333_333_333_333_313_164_28e-2;
const Q2: f64 = 1.587_301_587_254_814_601_65e-3;
const Q3: f64 = -7.936_507_578_674_879_424_73e-5;
const Q4: f64 = 4.008_217_827_329_362_395_52e-6;
const Q5: f64 = -2.010_992_181_836_243_713_26e-7;

/// `exp(x) - 1`, computed without the cancellation that spelling it that way would cause.
///
/// For small `x`, `exp(x)` is a number just above 1 and subtracting 1 throws away every bit
/// that carried the answer: at `x = 1e-17`, `exp(x) - 1.0` evaluates to exactly zero while the
/// true value is `1e-17`. This computes the difference directly and keeps full precision all
/// the way down. [`sinh`] and [`tanh`] are built on it for the same reason.
pub fn expm1(mut x: f64) -> f64 {
    let huge = f64::from_bits(0x7fe0_0000_0000_0000);
    let bits = x.to_bits();
    let sign = bits >> 63 == 1;
    let hx = (bits >> 32) as u32 & 0x7fff_ffff;

    if hx >= 0x4043_687a {
        // |x| >= 56*ln(2).
        if x.is_nan() {
            return x;
        }
        if sign {
            // exp(x) is below 2^-56 here, so exp(x) - 1 rounds to exactly -1.
            return -1.0;
        }
        if x > 709.782_712_893_383_973_096 {
            return x * huge;
        }
    }

    let (mut k, mut c) = (0, 0.0);
    if hx > 0x3fd6_2e42 {
        let (hi, lo);
        if hx < 0x3ff0_a2b2 {
            // 0.5*ln2 < |x| < 1.5*ln2.
            if sign {
                hi = x + LN2_HI;
                lo = -LN2_LO;
                k = -1;
            } else {
                hi = x - LN2_HI;
                lo = LN2_LO;
                k = 1;
            }
        } else {
            k = (INV_LN2 * x + if sign { -0.5 } else { 0.5 }) as i32;
            hi = x - k as f64 * LN2_HI;
            lo = k as f64 * LN2_LO;
        }
        x = hi - lo;
        // What the subtraction above rounded away. Unlike `exp`, this function's answer is
        // near zero rather than near one, so that error is not negligible against it.
        c = (hi - x) - lo;
    } else if hx < 0x3c90_0000 {
        // |x| < 2^-54: exp(x) - 1 and x agree to every bit.
        return x;
    }

    let half_x = 0.5 * x;
    let hxs = x * half_x;
    let r1 = 1.0 + hxs * (Q1 + hxs * (Q2 + hxs * (Q3 + hxs * (Q4 + hxs * Q5))));
    let t = 3.0 - r1 * half_x;
    let mut e = hxs * ((r1 - t) / (6.0 - x * t));
    if k == 0 {
        return x - (x * e - hxs);
    }
    e = x * (e - c) - c - hxs;

    // Reassemble 2^k * (1 + x - e) - 1. Each branch below exists because the generic form
    // loses the answer somewhere: near k = ±1 to cancellation against the 1, and at large |k|
    // because 2^-k has underflowed.
    if k == -1 {
        return 0.5 * (x - e) - 0.5;
    }
    if k == 1 {
        return if x < -0.25 {
            -2.0 * (e - (x + 0.5))
        } else {
            1.0 + 2.0 * (x - e)
        };
    }
    let two_k = f64::from_bits(((0x3ff + k) as u64) << 52);
    if !(0..=56).contains(&k) {
        // The `-1` is lost in the rounding anyway, so exp(x) - 1 and exp(x) agree.
        let y = x - e + 1.0;
        let y = if k == 1024 {
            y * 2.0 * f64::from_bits(0x7fe0_0000_0000_0000)
        } else {
            y * two_k
        };
        return y - 1.0;
    }
    let two_minus_k = f64::from_bits(((0x3ff - k) as u64) << 52);
    if k < 20 {
        (x - e + (1.0 - two_minus_k)) * two_k
    } else {
        (x - e - two_minus_k + 1.0) * two_k
    }
}

/// `exp(x) / 2`, for arguments large enough that `exp(x)` itself would overflow.
///
/// Splitting the scaling in two — `exp(x - k*ln2)` then two multiplications by `2^(k/2)` — keeps
/// every intermediate finite. `k` is odd, so `scale*scale` is `2^(k-1)` and not `2^k`, which is
/// exactly the halving wanted.
fn expo2(x: f64) -> f64 {
    const K: i32 = 2043;
    /// `K * ln(2)`, to full precision.
    const K_LN2: f64 = 1.416_582_454_492_007_5e3;
    let scale = f64::from_bits(((0x3ff + K / 2) as u64) << 52);
    exp(x - K_LN2) * scale * scale
}

/// The hyperbolic sine of `x`.
pub fn sinh(x: f64) -> f64 {
    let half = 0.5_f64.copysign(x);
    let magnitude = x.abs();
    let w = (magnitude.to_bits() >> 32) as u32;

    if w < 0x4086_2e42 {
        // |x| < ln(f64::MAX), so exp does not overflow.
        let t = expm1(magnitude);
        if w < 0x3ff0_0000 {
            if w < 0x3ff0_0000 - (26 << 20) {
                // |x| < 2^-26: sinh(x) and x agree to every bit.
                return x;
            }
            // 2*t - t*t/(t+1) rather than t + t/(t+1): below 1 the two differ in the last
            // bits, and this form is the one that stays accurate as x approaches zero.
            return half * (2.0 * t - t * t / (t + 1.0));
        }
        return half * (t + t / (t + 1.0));
    }
    // |x| >= ln(f64::MAX), or NaN. sinh and exp/2 have converged by here.
    2.0 * half * expo2(magnitude)
}

/// The hyperbolic cosine of `x`.
pub fn cosh(x: f64) -> f64 {
    let x = x.abs();
    let w = (x.to_bits() >> 32) as u32;

    if w < 0x3fe6_2e42 {
        // |x| < ln(2).
        if w < 0x3ff0_0000 - (26 << 20) {
            return 1.0;
        }
        let t = expm1(x);
        // 1 + t^2/(2(1+t)) keeps the small correction separate from the 1, which
        // 0.5*(e^x + e^-x) would lose entirely near zero.
        return 1.0 + t * t / (2.0 * (1.0 + t));
    }
    if w < 0x4086_2e42 {
        let t = exp(x);
        return 0.5 * (t + 1.0 / t);
    }
    expo2(x)
}

/// The hyperbolic tangent of `x`.
pub fn tanh(x: f64) -> f64 {
    let sign = x.is_sign_negative();
    let x = x.abs();
    let w = (x.to_bits() >> 32) as u32;

    let t = if w > 0x3fe1_93ea {
        // |x| > ln(3)/2, or NaN.
        if w > 0x4034_0000 {
            // |x| > 20: tanh has reached 1 to every bit f64 has. Written as `1 - 0/x` so a NaN
            // argument stays a NaN and an infinite one gives exactly 1.
            1.0 - 0.0 / x
        } else {
            let t = expm1(2.0 * x);
            1.0 - 2.0 / (t + 2.0)
        }
    } else if w > 0x3c6c_9866 {
        // |x| > 2^-56.
        let t = expm1(2.0 * x);
        t / (t + 2.0)
    } else {
        // Subnormal and just below: tanh(x) and x agree.
        x
    };
    if sign {
        -t
    } else {
        t
    }
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
    fn exp_hits_the_values_that_have_names() {
        assert_eq!(exp(0.0), 1.0);
        // Within a unit in the last place of e, not on it. fdlibm's exp is not correctly
        // rounded and does not claim to be; `tests/accuracy.rs` measures how far it strays.
        assert!((exp(1.0) - std::f64::consts::E).abs() <= f64::EPSILON * std::f64::consts::E);
        assert_eq!(exp(f64::INFINITY), f64::INFINITY);
        assert_eq!(exp(f64::NEG_INFINITY), 0.0);
        assert!(exp(f64::NAN).is_nan());
        // Overflow and underflow, and the subnormal band between the two thresholds.
        assert_eq!(exp(710.0), f64::INFINITY);
        assert_eq!(exp(-746.0), 0.0);
        assert!(exp(-730.0) > 0.0 && exp(-730.0) < f64::MIN_POSITIVE);
    }

    #[test]
    fn exp2_is_exact_on_integers_rather_than_merely_close() {
        for k in -1000..=1000 {
            let want = if k >= -1022 {
                f64::from_bits(((0x3ff + k) as u64) << 52)
            } else {
                crate::scalbn(1.0, k)
            };
            assert_eq!(exp2(k as f64).to_bits(), want.to_bits(), "2^{k}");
        }
        // Exactness is claimed for integers only. Half an exponent is an ordinary irrational
        // result and lands within a unit in the last place, like everything else here.
        assert!((exp2(0.5) - std::f64::consts::SQRT_2).abs() <= f64::EPSILON);
        assert_eq!(exp2(1024.0), f64::INFINITY);
        assert_eq!(exp2(-1076.0), 0.0);
        assert!(exp2(f64::NAN).is_nan());
    }

    /// The point of the function: `exp(x) - 1.0` is zero here, and the answer is not.
    #[test]
    fn expm1_keeps_the_answer_where_subtracting_one_would_destroy_it() {
        let x = 1e-17;
        assert_eq!(exp(x) - 1.0, 0.0);
        assert_eq!(expm1(x), x);

        assert_eq!(expm1(0.0), 0.0);
        assert!(expm1(-0.0).is_sign_negative());
        assert_eq!(expm1(f64::NEG_INFINITY), -1.0);
        assert_eq!(expm1(f64::INFINITY), f64::INFINITY);
        assert!(expm1(f64::NAN).is_nan());
        // Far enough negative that exp(x) rounds to 0 and the answer saturates at -1.
        assert_eq!(expm1(-100.0), -1.0);
    }

    #[test]
    fn the_hyperbolics_are_odd_or_even_as_they_should_be_and_survive_their_extremes() {
        for &x in &[0.3, 1.0, 5.0, 30.0, 700.0, 800.0] {
            assert_eq!(sinh(-x), -sinh(x), "sinh is odd at {x}");
            assert_eq!(cosh(-x).to_bits(), cosh(x).to_bits(), "cosh is even at {x}");
            assert_eq!(tanh(-x), -tanh(x), "tanh is odd at {x}");
        }
        assert_eq!(sinh(0.0), 0.0);
        assert!(sinh(-0.0).is_sign_negative());
        assert_eq!(cosh(0.0), 1.0);
        assert_eq!(tanh(0.0), 0.0);
        assert!(tanh(-0.0).is_sign_negative());

        // Past the point where exp overflows, sinh and cosh must still be finite for a while —
        // this is what `expo2` exists for.
        assert!(sinh(710.0).is_finite());
        assert!(cosh(710.0).is_finite());
        assert_eq!(sinh(f64::INFINITY), f64::INFINITY);
        assert_eq!(cosh(f64::NEG_INFINITY), f64::INFINITY);
        assert_eq!(tanh(f64::INFINITY), 1.0);
        assert_eq!(tanh(f64::NEG_INFINITY), -1.0);
        assert!(tanh(f64::NAN).is_nan());
        assert!(sinh(f64::NAN).is_nan());
        assert!(cosh(f64::NAN).is_nan());
    }
}
