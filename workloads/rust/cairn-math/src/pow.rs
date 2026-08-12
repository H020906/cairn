//! `x` raised to the power `y`.
//!
//! This is the longest function in the crate and the only one that could not be shortened
//! without giving something up. Two things make it hard.
//!
//! **The special cases are the specification.** `pow` has more of them than any other function
//! in libm, and most are not conveniences — they are required results that the general
//! algorithm gets wrong. `pow(1.0, NaN)` is `1.0`. `pow(NaN, 0.0)` is `1.0`. `pow(-1.0, inf)` is
//! `1.0`. `pow(-8.0, 3.0)` is `-512.0`, but `pow(-8.0, 3.5)` is NaN, so the code has to decide
//! whether `y` is an integer and whether it is odd — for a `y` that may be `2^40`. Roughly the
//! first third of the body is that decision table.
//!
//! **The precision demand scales with `y`.** `pow(x, y)` is `2^(y * log2(x))`, and an error of
//! `e` in `log2(x)` becomes an error of `y*e` in the exponent. For `y` near `2^31` that
//! multiplies a relative error by two billion, so a `log2` accurate to the 53 bits a `f64` holds
//! is not close to enough. The logarithm below is therefore computed and carried in two pieces
//! throughout — the repeated clearing of low bits is what keeps the head of each pair exactly
//! representable so the tail can hold everything else.
//!
//! The algorithm is fdlibm's, unchanged.

use crate::roots::sqrt;
use crate::{scalbn, LN2};

/// The two reduction points for the logarithm: `log2` is computed relative to whichever of
/// `1.0` and `1.5` the argument is nearer.
const BP: [f64; 2] = [1.0, 1.5];
/// `log2(BP[k])`, as heads.
const DP_H: [f64; 2] = [0.0, 5.849_624_872_207_641_601_56e-1];
/// The tails of [`DP_H`].
const DP_L: [f64; 2] = [0.0, 1.350_039_202_129_748_971_28e-8];

/// Minimax coefficients for `(3/2)*(log(x) - 2s - (2/3)s³)`, from fdlibm.
const L1: f64 = 5.999_999_999_999_946_487_25e-1;
const L2: f64 = 4.285_714_285_785_501_842_52e-1;
const L3: f64 = 3.333_333_298_183_774_329_18e-1;
const L4: f64 = 2.727_281_238_085_340_064_89e-1;
const L5: f64 = 2.306_607_457_755_617_540_67e-1;
const L6: f64 = 2.069_750_178_003_384_177_84e-1;

/// Minimax coefficients for the final `2^r`, the same ones [`exp`] uses.
const P1: f64 = 1.666_666_666_666_660_190_37e-1;
const P2: f64 = -2.777_777_777_701_559_338_42e-3;
const P3: f64 = 6.613_756_321_437_934_361_17e-5;
const P4: f64 = -1.653_390_220_546_525_153_90e-6;
const P5: f64 = 4.138_136_797_057_238_460_39e-8;

/// `ln(2)` split for the final exponential, with a different cut than [`crate::LN2_HI`] because
/// what must stay exact here is a different product.
const LG2_H: f64 = 6.931_471_824_645_996_093_75e-1;
/// The tail of [`LG2_H`].
const LG2_L: f64 = -1.904_654_299_957_768_045_25e-9;
/// `2/(3*ln(2))`, and its split form.
const CP: f64 = 9.617_966_939_259_755_543_29e-1;
/// The head of [`CP`].
const CP_H: f64 = 9.617_967_009_544_372_558_59e-1;
/// The tail of [`CP_H`], relative to [`CP`].
const CP_L: f64 = -7.028_461_650_952_758_265_16e-9;
/// `1/ln(2)`, and its split form, for the near-one shortcut.
const IVLN2: f64 = core::f64::consts::LOG2_E;
/// The head of [`IVLN2`], with 21 significant bits.
const IVLN2_H: f64 = 1.442_695_021_629_333_496_09e0;
/// The tail of [`IVLN2_H`].
const IVLN2_L: f64 = 1.925_962_991_126_617_468_87e-8;
/// `-(1024 - log2(largest finite + half an ulp))`, the margin used to decide whether an
/// exponent landing exactly on 1024 overflows or not.
const OVT: f64 = 8.008_566_259_537_294_437_2e-17;

/// A value whose square overflows, used to produce infinity through a multiplication so the
/// overflow is raised rather than merely arrived at.
const HUGE: f64 = 1.0e300;
/// The same, downward.
const TINY: f64 = 1.0e-300;

fn high(x: f64) -> u32 {
    (x.to_bits() >> 32) as u32
}
fn low(x: f64) -> u32 {
    x.to_bits() as u32
}
fn from_high(h: u32) -> f64 {
    f64::from_bits(u64::from(h) << 32)
}
fn with_high(x: f64, h: u32) -> f64 {
    f64::from_bits((u64::from(h) << 32) | (x.to_bits() & 0xffff_ffff))
}
/// Drops the low 32 bits, leaving a value with at most 21 significant bits.
fn head(x: f64) -> f64 {
    f64::from_bits(x.to_bits() & 0xffff_ffff_0000_0000)
}

/// Whether `y` is an integer, and if so whether it is odd.
///
/// Only asked when `x` is negative, which is the one case where it changes the answer: an odd
/// integer power of a negative number is negative, an even one positive, and a non-integral one
/// does not exist in the reals.
fn integrality(iy: u32, ly: u32) -> Parity {
    if iy >= 0x4340_0000 {
        // |y| >= 2^52: every representable value at this magnitude is an even integer, because
        // the last bit of the mantissa is already worth two or more.
        return Parity::Even;
    }
    if iy < 0x3ff0_0000 {
        // |y| < 1 and nonzero, so not an integer.
        return Parity::No;
    }
    let k = (iy >> 20) - 0x3ff;
    if k > 20 {
        // The fraction, if any, lives in the low word.
        let j = ly >> (52 - k);
        if j << (52 - k) == ly {
            return if j & 1 == 1 {
                Parity::Odd
            } else {
                Parity::Even
            };
        }
    } else if ly == 0 {
        let j = iy >> (20 - k);
        if j << (20 - k) == iy {
            return if j & 1 == 1 {
                Parity::Odd
            } else {
                Parity::Even
            };
        }
    }
    Parity::No
}

/// What [`integrality`] found.
#[derive(PartialEq, Eq, Clone, Copy)]
enum Parity {
    /// Not an integer at all.
    No,
    /// An odd integer.
    Odd,
    /// An even integer.
    Even,
}

/// `x` raised to the power `y`.
///
/// Follows IEEE-754's table of special cases exactly, including the ones that look like
/// mistakes: `pow(1.0, NaN)` is `1.0` and `pow(NaN, 0.0)` is `1.0`, because one raised to
/// anything and anything raised to zero are defined before the argument is even examined.
#[allow(clippy::indexing_slicing)]
pub fn pow(x: f64, y: f64) -> f64 {
    let (hx, lx) = (high(x) as i32, low(x));
    let (hy, ly) = (high(y) as i32, low(y));
    let mut ix = (hx & 0x7fff_ffff) as u32;
    let iy = (hy & 0x7fff_ffff) as u32;

    // Anything to the zero is one, including a NaN.
    if iy | ly == 0 {
        return 1.0;
    }
    // One to anything is one, including a NaN.
    if hx == 0x3ff0_0000 && lx == 0 {
        return 1.0;
    }
    // Any other NaN propagates.
    if ix > 0x7ff0_0000
        || (ix == 0x7ff0_0000 && lx != 0)
        || iy > 0x7ff0_0000
        || (iy == 0x7ff0_0000 && ly != 0)
    {
        return x + y;
    }

    let odd = if hx < 0 {
        integrality(iy, ly)
    } else {
        Parity::No
    };

    // Special values of y that have exact answers.
    if ly == 0 {
        if iy == 0x7ff0_0000 {
            // y is ±infinity.
            if ix == 0x3ff0_0000 && lx == 0 {
                // (±1)^±inf is 1 — the magnitude never moves, so the limit does not exist and
                // the standard picks one.
                return 1.0;
            } else if ix >= 0x3ff0_0000 {
                return if hy >= 0 { y } else { 0.0 };
            }
            return if hy >= 0 { 0.0 } else { -y };
        }
        if iy == 0x3ff0_0000 {
            return if hy >= 0 { x } else { 1.0 / x };
        }
        if hy == 0x4000_0000 {
            return x * x;
        }
        if hy == 0x3fe0_0000 && hx >= 0 {
            // A square root is both faster and correctly rounded, which the general path is
            // not.
            return sqrt(x);
        }
    }

    let mut ax = x.abs();
    // Special values of x: ±0, ±inf, ±1, where the answer is exact.
    if lx == 0 && (ix == 0x7ff0_0000 || ix == 0 || ix == 0x3ff0_0000) {
        let mut z = ax;
        if hy < 0 {
            z = 1.0 / z;
        }
        if hx < 0 {
            if ix == 0x3ff0_0000 && odd == Parity::No {
                // A non-integral power of -1 does not exist.
                z = crate::invalid(z);
            } else if odd == Parity::Odd {
                z = -z;
            }
        }
        return z;
    }

    // The sign of the result: negative only for an odd integer power of a negative base.
    let sign = if hx < 0 {
        match odd {
            Parity::No => return crate::invalid(x),
            Parity::Odd => -1.0,
            Parity::Even => 1.0,
        }
    } else {
        1.0
    };

    // `t1 + t2` will hold log2(|x|) in two pieces.
    let (t1, t2);

    if iy > 0x41e0_0000 {
        // |y| > 2^31. The result almost certainly overflows or underflows, and the only way it
        // does not is if |x| is within 2^-20 of one — in which case log2(x) is small enough to
        // get from four terms of its series.
        if iy > 0x43f0_0000 {
            // |y| > 2^64: nothing can survive.
            if ix <= 0x3fef_ffff {
                return if hy < 0 { HUGE * HUGE } else { TINY * TINY };
            }
            if ix >= 0x3ff0_0000 {
                return if hy > 0 { HUGE * HUGE } else { TINY * TINY };
            }
        }
        if ix < 0x3fef_ffff {
            return if hy < 0 {
                sign * HUGE * HUGE
            } else {
                sign * TINY * TINY
            };
        }
        if ix > 0x3ff0_0000 {
            return if hy > 0 {
                sign * HUGE * HUGE
            } else {
                sign * TINY * TINY
            };
        }
        // |1 - x| <= 2^-20, so log(x) = t - t²/2 + t³/3 - t⁴/4 is enough.
        let t = ax - 1.0;
        let w = (t * t) * (0.5 - t * (0.333_333_333_333_333_333_33 - t * 0.25));
        let u = IVLN2_H * t;
        let v = t * IVLN2_L - w * IVLN2;
        let head_of = head(u + v);
        t1 = head_of;
        t2 = v - (head_of - u);
    } else {
        let mut n;
        if ix < 0x0010_0000 {
            // Subnormal: scale into range and account for it in the exponent.
            ax *= 9_007_199_254_740_992.0;
            n = -53;
            ix = high(ax);
        } else {
            n = 0;
        }
        n += ((ix >> 20) as i32) - 0x3ff;
        let j = ix & 0x000f_ffff;
        ix = j | 0x3ff0_0000;

        // Pick the reduction point, and adjust the exponent if the mantissa is closer to 1.5
        // than to either 1 or 3.
        let k;
        if j <= 0x3_988e {
            k = 0;
        } else if j < 0xb_b67a {
            k = 1;
        } else {
            k = 0;
            n += 1;
            ix -= 0x0010_0000;
        }
        ax = with_high(ax, ix);

        // s = (ax - bp)/(ax + bp), in two pieces.
        let u = ax - BP[k];
        let v = 1.0 / (ax + BP[k]);
        let ss = u * v;
        let s_h = head(ss);
        // The high half of ax + bp, built from the exponent so it is exactly representable.
        let t_h = from_high(((ix >> 1) | 0x2000_0000) + 0x0008_0000 + ((k as u32) << 18));
        let t_l = ax - (t_h - BP[k]);
        let s_l = v * ((u - s_h * t_h) - s_h * t_l);

        // log(ax) from the odd-power series in s.
        let s2 = ss * ss;
        let mut r = s2 * s2 * (L1 + s2 * (L2 + s2 * (L3 + s2 * (L4 + s2 * (L5 + s2 * L6)))));
        r += s_l * (s_h + ss);
        let s2 = s_h * s_h;
        let t_h = head(3.0 + s2 + r);
        let t_l = r - ((t_h - 3.0) - s2);

        let u = s_h * t_h;
        let v = s_l * t_h + t_l * ss;
        let p_h = head(u + v);
        let p_l = v - (p_h - u);
        let z_h = CP_H * p_h;
        let z_l = CP_L * p_h + p_l * CP + DP_L[k];

        // log2(ax) = n + DP_H[k] + z_h + z_l, split so the exponent's own magnitude cannot
        // swamp the fraction.
        let t = n as f64;
        let head_of = head(((z_h + z_l) + DP_H[k]) + t);
        t1 = head_of;
        t2 = z_l - (((head_of - t) - DP_H[k]) - z_h);
    }

    // (y1 + y2) * (t1 + t2), with y split the same way.
    let y1 = head(y);
    let p_l = (y - y1) * t1 + y * t2;
    let mut p_h = y1 * t1;
    let z = p_l + p_h;
    let (j, i) = (high(z) as i32, low(z));

    if j >= 0x4090_0000 {
        // z >= 1024, the largest exponent f64 has.
        if j != 0x4090_0000 || i != 0 {
            return sign * HUGE * HUGE;
        }
        // Exactly 1024: whether it overflows depends on bits below what `z` kept.
        if p_l + OVT > z - p_h {
            return sign * HUGE * HUGE;
        }
    } else if (j & 0x7fff_ffff) >= 0x4090_cc00 {
        // z <= -1075, past the smallest subnormal. The constant is `0xc090cc00` read as a
        // signed word, which is what `z == -1075` looks like in bits.
        if j != -0x3f6f_3400 || i != 0 {
            return sign * TINY * TINY;
        }
        if p_l <= z - p_h {
            return sign * TINY * TINY;
        }
    }

    // 2^(p_h + p_l), by splitting off the integer part and exponentiating the rest.
    let i = (j & 0x7fff_ffff) as u32;
    let mut k = ((i >> 20) as i32) - 0x3ff;
    let mut n = 0i32;
    if i > 0x3fe0_0000 {
        // |z| > 0.5, so there is an integer part to remove.
        let m = j + (0x0010_0000 >> (k + 1));
        k = ((m & 0x7fff_ffff) >> 20) - 0x3ff;
        let t = from_high((m as u32) & !(0x000f_ffff >> k));
        n = ((m & 0x000f_ffff) | 0x0010_0000) >> (20 - k);
        if j < 0 {
            n = -n;
        }
        p_h -= t;
    }
    let t = head(p_l + p_h);
    let u = t * LG2_H;
    let v = (p_l - (t - p_h)) * LN2 + t * LG2_L;
    let mut z = u + v;
    let w = v - (z - u);
    let t = z * z;
    let t1 = z - t * (P1 + t * (P2 + t * (P3 + t * (P4 + t * P5))));
    let r = (z * t1) / (t1 - 2.0) - (w + z * w);
    z = 1.0 - (r - z);
    let j = (high(z) as i32) + (n << 20);
    if j >> 20 <= 0 {
        // The result is subnormal, so the exponent cannot simply be written in.
        z = scalbn(z, n);
    } else {
        z = with_high(z, j as u32);
    }
    sign * z
}

/// A convenience for the one case `pow` cannot do well: `exp` composed with a logarithm.
///
/// Not exported. Present so the special-case table above can be read against something.
#[cfg(test)]
fn reference(x: f64, y: f64) -> f64 {
    crate::exp(y * crate::ln(x))
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

    /// The constants that have closed forms are checked against them. A transcription error in
    /// one of these would show up as a small accuracy loss rather than a wrong answer, which is
    /// exactly the kind of bug that survives a test suite.
    #[test]
    fn the_constants_are_the_numbers_they_claim_to_be() {
        // Two units in the last place: these are deliberately rounded constants, and the claim
        // is that each is the number it is named after, not that it is that number exactly.
        let close = |a: f64, b: f64, what: &str| {
            assert!(
                (a - b).abs() <= 2.0 * f64::EPSILON * b.abs(),
                "{what}: {a:e} vs {b:e}"
            );
        };
        close(CP, 2.0 / (3.0 * LN2), "cp = 2/(3 ln 2)");
        close(IVLN2, 1.0 / LN2, "1/ln 2");
        close(LG2_H + LG2_L, LN2, "lg2 split");
        close(CP_H + CP_L, CP, "cp split");
        close(IVLN2_H + IVLN2_L, IVLN2, "1/ln2 split");
        close(DP_H[1] + DP_L[1], 1.5_f64.log2(), "log2(1.5)");
        // Each head must have its low 29 bits clear — 24 significant bits, the width of a
        // `f32` — or the products built on it stop being exact.
        for (name, v) in [
            ("lg2_h", LG2_H),
            ("cp_h", CP_H),
            ("ivln2_h", IVLN2_H),
            ("dp_h", DP_H[1]),
        ] {
            assert_eq!(v.to_bits() & 0x1fff_ffff, 0, "{name} is not a clean head");
        }
    }

    /// IEEE-754's table, which is most of what makes this function long.
    #[test]
    fn the_special_cases_are_the_ones_the_standard_names() {
        // Zero and one win over everything, including NaN.
        assert_eq!(pow(f64::NAN, 0.0), 1.0);
        assert_eq!(pow(1.0, f64::NAN), 1.0);
        assert_eq!(pow(0.0, 0.0), 1.0);
        assert_eq!(pow(f64::INFINITY, 0.0), 1.0);
        // Magnitude one against an infinite exponent.
        assert_eq!(pow(-1.0, f64::INFINITY), 1.0);
        assert_eq!(pow(-1.0, f64::NEG_INFINITY), 1.0);
        // Negative bases: integrality of the exponent decides everything.
        assert_eq!(pow(-8.0, 3.0), -512.0);
        assert_eq!(pow(-8.0, 2.0), 64.0);
        assert!(pow(-8.0, 3.5).is_nan());
        assert!(pow(-1.0, 0.5).is_nan());
        // A y large enough that every representable value is an even integer.
        assert_eq!(pow(-1.0, 1e300), 1.0);
        // Signed zeros, which follow the odd/even rule too.
        assert_eq!(pow(-0.0, 3.0), -0.0);
        assert_eq!(pow(-0.0, 2.0), 0.0);
        assert_eq!(pow(0.0, -1.0), f64::INFINITY);
        assert_eq!(pow(-0.0, -3.0), f64::NEG_INFINITY);
        // Infinities.
        assert_eq!(pow(f64::INFINITY, 2.0), f64::INFINITY);
        assert_eq!(pow(f64::INFINITY, -2.0), 0.0);
        assert_eq!(pow(f64::NEG_INFINITY, 3.0), f64::NEG_INFINITY);
        assert_eq!(pow(2.0, f64::INFINITY), f64::INFINITY);
        assert_eq!(pow(0.5, f64::INFINITY), 0.0);
        // Overflow and underflow.
        assert_eq!(pow(10.0, 400.0), f64::INFINITY);
        assert_eq!(pow(10.0, -400.0), 0.0);
        assert!(pow(f64::NAN, 2.0).is_nan());
    }

    #[test]
    fn integer_powers_come_out_exact() {
        // Small integer powers of two, where there is an exactly right answer to hit.
        for base in [2.0_f64, 4.0, 0.5, -2.0] {
            let mut want = 1.0_f64;
            for e in 0..40 {
                assert_eq!(pow(base, e as f64), want, "{base}^{e}");
                want *= base;
            }
        }
        assert_eq!(pow(9.0, 0.5), 3.0);
        assert_eq!(pow(3.0, 2.0), 9.0);
        assert_eq!(pow(10.0, 22.0), 1e22);
    }

    /// The property the two-piece logarithm exists for.
    ///
    /// `pow(3, 500)` is an exact integer, so there is a right answer. Computing it as
    /// `exp(500 * ln(3))` gives a `ln(3)` correct to its own last bit and then multiplies that
    /// error by five hundred, which lands hundreds of units in the last place away. The careful
    /// path carries the logarithm in two pieces precisely so this does not happen.
    #[test]
    fn a_large_exponent_does_not_multiply_up_an_error_in_the_logarithm() {
        let (x, y) = (3.0_f64, 500.0_f64);
        let want = x.powf(y);
        let ulp = f64::from_bits(want.to_bits() + 1) - want;

        let mine = pow(x, y);
        let off = (mine - want).abs() / ulp;
        assert!(
            off <= 2.0,
            "pow(3, 500) is {off} ulp out: {mine:e} vs {want:e}"
        );

        // The naive composition, for contrast. The assertion is that it is *much* worse, so a
        // future change that quietly turned `pow` into `exp(y*ln(x))` would fail here.
        let naive = reference(x, y);
        let drift = (naive - want).abs() / ulp;
        assert!(
            drift > 100.0,
            "exp(y*ln(x)) was supposed to be far off, but was {drift} ulp"
        );
    }
}
