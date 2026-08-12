//! The inverse trigonometric functions.
//!
//! [`asin`] and [`acos`] share a rational approximation to `(asin(x) - x)/x³` that is only good
//! on `|x| <= 0.5`. Above that they use `asin(x) = pi/2 - 2*asin(sqrt((1-x)/2))`, which trades
//! the argument for a small one — at the price of a `sqrt` whose own rounding error would
//! otherwise land squarely in the answer. The correction terms named `c` below are that
//! rounding error, recovered exactly and added back.
//!
//! [`atan`] instead reduces by subtraction against four fixed points. `atan(x)` for `x` near
//! one of `0.5`, `1`, `1.5`, `inf` is `atan(that) + atan(small)`, and the four constants are
//! each stored as a head and a tail so the addition does not spend the precision the
//! polynomial just earned.

use crate::roots::sqrt;

/// `pi/2`, and the part of it that does not fit in a `f64`.
///
/// Taken from `core` rather than written out: it is the same value, and a constant that does
/// not have to be transcribed cannot be transcribed wrongly. The tail below is the part that
/// has no name in `core`, and it is the reason this pair exists at all.
const PIO2_HI: f64 = core::f64::consts::FRAC_PI_2;
/// The tail of [`PIO2_HI`].
const PIO2_LO: f64 = 6.123_233_995_736_766_035_87e-17;
/// `pi`, and its tail, for [`atan2`]'s reflections.
const PI: f64 = core::f64::consts::PI;
/// The tail of [`PI`].
const PI_LO: f64 = 1.224_646_799_147_353_177_2e-16;

/// Numerator coefficients of the rational approximation to `(asin(x) - x)/x³`, from fdlibm.
const PS0: f64 = 1.666_666_666_666_666_574_15e-1;
const PS1: f64 = -3.255_658_186_224_009_154_05e-1;
const PS2: f64 = 2.012_125_321_348_629_258_81e-1;
const PS3: f64 = -4.005_553_450_067_941_140_27e-2;
const PS4: f64 = 7.915_349_942_898_145_321_76e-4;
const PS5: f64 = 3.479_331_075_960_211_675_7e-5;
/// Denominator coefficients; the constant term is 1 and is written out at the use site.
const QS1: f64 = -2.403_394_911_734_414_218_78e0;
const QS2: f64 = 2.020_945_760_233_505_694_71e0;
const QS3: f64 = -6.882_839_716_054_532_930_3e-1;
const QS4: f64 = 7.703_815_055_590_193_527_91e-2;

/// The shared rational approximation, in terms of `z = x²`.
fn r(z: f64) -> f64 {
    let p = z * (PS0 + z * (PS1 + z * (PS2 + z * (PS3 + z * (PS4 + z * PS5)))));
    let q = 1.0 + z * (QS1 + z * (QS2 + z * (QS3 + z * QS4)));
    p / q
}

/// Zeroes the low 32 bits, leaving a value that squares exactly.
fn head(x: f64) -> f64 {
    f64::from_bits(x.to_bits() & 0xffff_ffff_0000_0000)
}

/// The arcsine of `x`, in radians. NaN outside `[-1, 1]`.
pub fn asin(x: f64) -> f64 {
    let hx = (x.to_bits() >> 32) as u32;
    let ix = hx & 0x7fff_ffff;

    if ix >= 0x3ff0_0000 {
        // |x| >= 1, or a NaN.
        if ix == 0x3ff0_0000 && x.to_bits() << 32 == 0 {
            return x * PIO2_HI;
        }
        return crate::invalid(x);
    }
    if ix < 0x3fe0_0000 {
        // |x| < 0.5: the approximation applies directly.
        if ix < 0x3e50_0000 {
            // |x| < 2^-26: asin(x) and x agree to every bit.
            return x;
        }
        return x + x * r(x * x);
    }

    // 0.5 <= |x| < 1. Reflect onto a small argument.
    let z = (1.0 - x.abs()) * 0.5;
    let s = sqrt(z);
    let value = if ix >= 0x3fef_3333 {
        // |x| > 0.975: the result is close enough to pi/2 that the sqrt's own error is below
        // the last bit and does not need recovering.
        PIO2_HI - (2.0 * (s + s * r(z)) - PIO2_LO)
    } else {
        // `f` squares exactly, so `c` is precisely what `sqrt` rounded away — and that error,
        // doubled, is comparable to the last bit of the answer.
        let f = head(s);
        let c = (z - f * f) / (s + f);
        0.5 * PIO2_HI - (2.0 * s * r(z) - (PIO2_LO - 2.0 * c) - (0.5 * PIO2_HI - 2.0 * f))
    };
    if hx >> 31 == 1 {
        -value
    } else {
        value
    }
}

/// The arccosine of `x`, in radians. NaN outside `[-1, 1]`.
pub fn acos(x: f64) -> f64 {
    let hx = (x.to_bits() >> 32) as u32;
    let ix = hx & 0x7fff_ffff;

    if ix >= 0x3ff0_0000 {
        if ix == 0x3ff0_0000 && x.to_bits() << 32 == 0 {
            // acos(1) is exactly zero; acos(-1) is pi.
            return if hx >> 31 == 1 { 2.0 * PIO2_HI } else { 0.0 };
        }
        return crate::invalid(x);
    }
    if ix < 0x3fe0_0000 {
        if ix <= 0x3c60_0000 {
            // |x| < 2^-57: acos(x) rounds to pi/2.
            return PIO2_HI;
        }
        // Nested so that `x` is subtracted from the tail rather than from pi/2, where it would
        // cancel against a much larger number.
        return PIO2_HI - (x - (PIO2_LO - x * r(x * x)));
    }
    if hx >> 31 == 1 {
        // x < -0.5: acos(x) = pi - 2*asin(sqrt((1+x)/2)).
        let z = (1.0 + x) * 0.5;
        let s = sqrt(z);
        return 2.0 * (PIO2_HI - (s + (r(z) * s - PIO2_LO)));
    }
    // x > 0.5: acos(x) = 2*asin(sqrt((1-x)/2)), with the sqrt's rounding recovered as in asin.
    let z = (1.0 - x) * 0.5;
    let s = sqrt(z);
    let f = head(s);
    let c = (z - f * f) / (s + f);
    2.0 * (f + (r(z) * s + c))
}

/// `atan` at the four reduction points, as heads.
const ATAN_HI: [f64; 4] = [
    4.636_476_090_008_060_935_15e-1,
    core::f64::consts::FRAC_PI_4,
    9.827_937_232_473_290_540_82e-1,
    core::f64::consts::FRAC_PI_2,
];
/// The tails of [`ATAN_HI`].
const ATAN_LO: [f64; 4] = [
    2.269_877_745_296_168_709_24e-17,
    3.061_616_997_868_383_017_93e-17,
    1.390_331_103_123_099_845_16e-17,
    6.123_233_995_736_766_035_87e-17,
];
/// Minimax coefficients for `atan(x)/x - 1`, from fdlibm.
const AT: [f64; 11] = [
    3.333_333_333_333_293_180_27e-1,
    -1.999_999_999_987_648_324_76e-1,
    1.428_571_427_250_346_637_11e-1,
    -1.111_111_040_546_235_578_8e-1,
    9.090_887_133_436_506_561_96e-2,
    -7.691_876_205_044_829_994_95e-2,
    6.661_073_137_387_531_206_69e-2,
    -5.833_570_133_790_573_486_45e-2,
    4.976_877_994_615_932_360_17e-2,
    -3.653_157_274_421_691_552_7e-2,
    1.628_582_011_536_578_236_23e-2,
];

/// The arctangent of `x`, in radians, in `(-pi/2, pi/2)`.
#[allow(clippy::indexing_slicing)]
pub fn atan(mut x: f64) -> f64 {
    let bits = (x.to_bits() >> 32) as u32;
    let negative = bits >> 31 == 1;
    let ix = bits & 0x7fff_ffff;

    if ix >= 0x4410_0000 {
        // |x| >= 2^66: atan has reached pi/2 to every bit f64 has.
        if x.is_nan() {
            return x;
        }
        return if negative { -ATAN_HI[3] } else { ATAN_HI[3] };
    }

    // Choose which of the four points to reduce against, and reduce.
    let region = if ix < 0x3fdc_0000 {
        // |x| < 0.4375: no reduction at all.
        if ix < 0x3e40_0000 {
            // |x| < 2^-27: atan(x) and x agree to every bit.
            return x;
        }
        None
    } else {
        x = x.abs();
        Some(if ix < 0x3ff3_0000 {
            if ix < 0x3fe6_0000 {
                x = (2.0 * x - 1.0) / (2.0 + x);
                0
            } else {
                x = (x - 1.0) / (x + 1.0);
                1
            }
        } else if ix < 0x4003_8000 {
            x = (x - 1.5) / (1.0 + 1.5 * x);
            2
        } else {
            x = -1.0 / x;
            3
        })
    };

    // The series has only odd powers, so it splits into two halves that share the work.
    let z = x * x;
    let w = z * z;
    let odd = z * (AT[0] + w * (AT[2] + w * (AT[4] + w * (AT[6] + w * (AT[8] + w * AT[10])))));
    let even = w * (AT[1] + w * (AT[3] + w * (AT[5] + w * (AT[7] + w * AT[9]))));

    match region {
        None => x - x * (odd + even),
        Some(id) => {
            let value = ATAN_HI[id] - ((x * (odd + even) - ATAN_LO[id]) - x);
            if negative {
                -value
            } else {
                value
            }
        }
    }
}

/// The angle from the positive x-axis to the point `(x, y)`, in `[-pi, pi]`.
///
/// Unlike `atan(y / x)`, this knows which quadrant the point is in, and it survives `x == 0`
/// and a quotient that would overflow or underflow.
pub fn atan2(y: f64, x: f64) -> f64 {
    if x.is_nan() || y.is_nan() {
        return x + y;
    }
    if x.to_bits() == 1.0_f64.to_bits() {
        return atan(y);
    }

    let (xb, yb) = (x.to_bits(), y.to_bits());
    // Two bits naming the quadrant: the sign of y, then the sign of x.
    let quadrant = ((yb >> 63) & 1) | ((xb >> 62) & 2);
    let (ix, iy) = (
        (xb >> 32) as u32 & 0x7fff_ffff,
        (yb >> 32) as u32 & 0x7fff_ffff,
    );
    let (x_zero, y_zero) = (xb << 1 == 0, yb << 1 == 0);

    if y_zero {
        // Along the x-axis. The sign of a zero y still decides which side of pi to return.
        return match quadrant {
            0 | 1 => y,
            2 => PI,
            _ => -PI,
        };
    }
    if x_zero {
        return if quadrant & 1 == 1 { -PIO2_HI } else { PIO2_HI };
    }
    if ix == 0x7ff0_0000 {
        return if iy == 0x7ff0_0000 {
            // Both infinite: the answer is a multiple of pi/4 and depends only on the signs.
            match quadrant {
                0 => PI / 4.0,
                1 => -PI / 4.0,
                2 => 3.0 * PI / 4.0,
                _ => -3.0 * PI / 4.0,
            }
        } else {
            match quadrant {
                0 => 0.0,
                1 => -0.0,
                2 => PI,
                _ => -PI,
            }
        };
    }
    // |y/x| beyond 2^64, or y infinite: the angle has reached the vertical.
    if ix.wrapping_add(64 << 20) < iy || iy == 0x7ff0_0000 {
        return if quadrant & 1 == 1 { -PIO2_HI } else { PIO2_HI };
    }

    // The quotient is formed only where it cannot overflow or flush to zero.
    let z = if quadrant & 2 != 0 && iy.wrapping_add(64 << 20) < ix {
        0.0
    } else {
        atan((y / x).abs())
    };
    match quadrant {
        0 => z,
        1 => -z,
        // Reflecting through pi needs pi to more than f64 precision, or the reflection itself
        // costs a bit of the answer.
        2 => PI - (z - PI_LO),
        _ => (z - PI_LO) - PI,
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
    fn the_named_values_come_out_named() {
        assert_eq!(asin(0.0), 0.0);
        assert!(asin(-0.0).is_sign_negative());
        assert_eq!(asin(1.0), PIO2_HI);
        assert_eq!(asin(-1.0), -PIO2_HI);
        assert!(asin(1.5).is_nan());
        assert!(asin(f64::NAN).is_nan());

        assert_eq!(acos(1.0), 0.0);
        assert_eq!(acos(-1.0), PI);
        assert_eq!(acos(0.0), PIO2_HI);
        assert!(acos(-1.5).is_nan());

        assert_eq!(atan(0.0), 0.0);
        assert!(atan(-0.0).is_sign_negative());
        assert_eq!(atan(1.0), std::f64::consts::FRAC_PI_4);
        assert_eq!(atan(f64::INFINITY), PIO2_HI);
        assert_eq!(atan(f64::NEG_INFINITY), -PIO2_HI);
        assert!(atan(f64::NAN).is_nan());
    }

    /// The whole reason `atan2` is not `atan(y / x)`.
    #[test]
    fn atan2_knows_which_quadrant_it_is_in() {
        assert_eq!(atan2(1.0, 1.0), std::f64::consts::FRAC_PI_4);
        assert_eq!(atan2(1.0, -1.0), 3.0 * std::f64::consts::FRAC_PI_4);
        assert_eq!(atan2(-1.0, -1.0), -3.0 * std::f64::consts::FRAC_PI_4);
        assert_eq!(atan2(-1.0, 1.0), -std::f64::consts::FRAC_PI_4);
        // The three cases `atan(y / x)` cannot answer at all.
        assert_eq!(atan2(1.0, 0.0), PIO2_HI);
        assert_eq!(atan2(-1.0, 0.0), -PIO2_HI);
        assert_eq!(atan2(0.0, -1.0), PI);
        // The sign of a zero decides which side of pi, which is why it is read from the bits.
        assert_eq!(atan2(-0.0, -1.0), -PI);
        assert_eq!(atan2(0.0, 1.0), 0.0);
        assert!(atan2(-0.0, 1.0).is_sign_negative());
        // Quotients that would overflow or flush to zero if formed.
        assert_eq!(atan2(1e300, 1e-300), PIO2_HI);
        assert_eq!(atan2(1e-300, -1e300), PI);
        // Infinities.
        assert_eq!(atan2(f64::INFINITY, f64::INFINITY), PI / 4.0);
        assert_eq!(atan2(f64::NEG_INFINITY, f64::NEG_INFINITY), -3.0 * PI / 4.0);
        assert!(atan2(f64::NAN, 1.0).is_nan());
    }

    #[test]
    fn the_inverses_undo_the_functions_they_invert() {
        let mut state = 0xdead_beef_cafe_babe_u64;
        let mut next = || {
            state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        };
        for _ in 0..20_000 {
            let x = (next() as f64 / u64::MAX as f64) * 2.0 - 1.0;
            assert!((crate::sin(asin(x)) - x).abs() < 4e-16, "sin(asin({x}))");
            assert!((crate::cos(acos(x)) - x).abs() < 4e-16, "cos(acos({x}))");
            // asin and acos must sum to a right angle, to within the error each carries.
            assert!(
                (asin(x) + acos(x) - PIO2_HI).abs() < 1e-15,
                "asin+acos at {x}"
            );
            let t = (next() as f64 / u64::MAX as f64) * 200.0 - 100.0;
            assert!(
                (crate::tan(atan(t)) - t).abs() < 1e-13 * t.abs().max(1.0),
                "tan(atan({t}))"
            );
        }
    }
}
