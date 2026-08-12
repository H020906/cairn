//! Sine, cosine and tangent, and the argument reduction they all depend on.
//!
//! # Why the reduction is the whole problem
//!
//! The polynomial kernels below are easy: on `|r| <= pi/4` a dozen coefficients give a result
//! good to a fraction of a unit in the last place. The hard part is getting there. To evaluate
//! `sin(x)` for large `x` you must first find `r = x - n*(pi/2)`, and `pi/2` is irrational, so
//! `n*(pi/2)` cannot be represented. Subtracting an approximation of it destroys the answer:
//! the result of the subtraction is nearly zero, and nearly all of its remaining bits came from
//! the error in the approximation rather than from `x`.
//!
//! How bad it gets is not a matter of taste. Among all `f64` values there are arguments where
//! `x*(2/pi)` sits within `2^-61` of an integer, so the reduction has to be carried out with
//! **more than 110 bits of `2/pi`** for the reduced value to have any correct bits at all. A
//! naive `x - n*PI_2` returns noise for those, and libraries that reduce with a two-term or
//! three-term split of `pi/2` return noise past the point where the split runs out — typically
//! somewhere above `2^20`, silently.
//!
//! **This is not a hypothetical, and the evidence turned up here rather than being looked
//! for.** For `x = 6381956970095103 * 2^797`, the true remainder is `4.687e-19` and `sin(x)` is
//! `1.0` to every bit a `f64` has. This crate returns `1.0`. V8 returns `1.0`. Exact integer
//! arithmetic over a 3000-bit `pi` returns `1.0`. The **platform libm these tests run against
//! returns `-0.2227`** — not a rounding difference, a wrong answer, from a shipping production
//! library, with nothing to indicate anything went wrong. See
//! `the_worst_case_in_the_format_comes_out_right_even_though_the_platform_gets_it_wrong`.
//!
//! # What this does instead
//!
//! [`rem_pio2`] holds `2/pi` as a table of exact 64-bit chunks and multiplies by `x`'s mantissa
//! **as integers**. There is no rounding anywhere in it, so there is no accuracy cliff at any
//! magnitude: `sin(1e300)` is computed by exactly the same code, to exactly the same quality,
//! as `sin(1.5)`. Only the last few bits of the integer part are kept, because only the
//! quadrant matters, and 384 bits of the fraction are kept, because the worst case needs 110
//! of them and the margin is nearly free.
//!
//! The cost is a handful of 128-bit multiplies, which is not obviously worse than the branchy
//! multi-step reduction it replaces, and there is only one code path — so there is no seam
//! between a fast case and a slow case for a workload to fall through.
//!
//! The table is not transcribed from anywhere. `the_table_is_two_over_pi` recomputes it from
//! Machin's formula with big-integer arithmetic and compares, so a wrong digit is a failing
//! test rather than a workload that quietly returns nonsense above `2^20`.

use crate::{dd_add, two_product, two_sum};

/// `pi/2`, and the part of it that does not fit.
///
/// Together these give `pi/2` to about 106 bits, which is what turns the exactly-reduced
/// fraction back into a `f64` argument without giving away the precision just bought.
const PIO2_HI: f64 = core::f64::consts::FRAC_PI_2;
/// The tail of [`PIO2_HI`].
const PIO2_LO: f64 = 6.123_233_995_736_766_035_87e-17;

/// The binary expansion of `2/pi`, in 64-bit chunks.
///
/// Chunk `j` holds bits `64j+1` through `64j+64` after the binary point. Twenty-four of them
/// cover every `f64` exponent with room to spare: the largest finite `f64` needs chunk 20.
///
/// Derived, not copied — see `the_table_is_two_over_pi`.
const TWO_OVER_PI: [u64; 24] = [
    0xa2f9_836e_4e44_1529,
    0xfc27_57d1_f534_ddc0,
    0xdb62_9599_3c43_9041,
    0xfe51_63ab_debb_c561,
    0xb724_6e3a_424d_d2e0,
    0x0649_2eea_09d1_921c,
    0xfe1d_eb1c_b129_a73e,
    0xe882_35f5_2ebb_4484,
    0xe99c_7026_b45f_7e41,
    0x3991_d639_8353_39f4,
    0x9c84_5f8b_bdf9_283b,
    0x1ff8_97ff_de05_980f,
    0xef2f_118b_5a0a_6d1f,
    0x6d36_7ecf_27cb_09b7,
    0x4f46_3f66_9e5f_ea2d,
    0x7527_bac7_ebe5_f17b,
    0x3d07_39f7_8a52_92ea,
    0x6bfb_5fb1_1f8d_5d08,
    0x5603_3046_fc7b_6bab,
    0xf0cf_bc20_9af4_361d,
    0xa9e3_9161_5ee6_1b08,
    0x6599_855f_14a0_6840,
    0x8dff_d880_4d73_2731,
    0x0606_1556_ca73_a8c9,
];

/// `2^e`, for the modest exponents the reduction produces.
fn pow2(e: i32) -> f64 {
    f64::from_bits(((0x3ff + e) as u64) << 52)
}

/// Splits `x` into a quadrant and a remainder: `x = n*(pi/2) + hi + lo`, with `|hi + lo| <= pi/4`.
///
/// Only `n mod 4` is meaningful; the higher bits are discarded during the reduction because
/// nothing downstream can use them. `x` must be finite and larger in magnitude than `pi/4` —
/// every caller checks both, because both have answers that skip this work entirely.
///
/// The arithmetic below is integer arithmetic on fixed-size arrays, and the indices are the
/// algorithm: which limb sits where relative to the binary point is the entire content of the
/// routine. Iterator adapters would hide exactly the thing a reader needs to check.
#[allow(clippy::indexing_slicing)]
fn rem_pio2(x: f64) -> (i32, f64, f64) {
    let bits = x.to_bits();
    let negative = bits >> 63 == 1;
    // |x| = mantissa * 2^exponent, with the implicit leading bit made explicit. Subnormals
    // cannot arrive here: they are far below pi/4.
    let mantissa = (bits & ((1 << 52) - 1)) | (1 << 52);
    let exponent = (((bits >> 52) & 0x7ff) as i32) - 1023 - 52;

    // Write the exponent as 64*chunk + shift. `div_euclid` rather than `/` because the
    // exponent is negative for every |x| below 2^52 and truncating division rounds the wrong
    // way there.
    let chunk = exponent.div_euclid(64);
    let shift = exponent.rem_euclid(64) as u32;

    // Seven chunks of 2/pi, least significant first. Chunks below the window contribute only
    // multiples of four to the integer part, which the quadrant does not distinguish; chunks
    // above it contribute below 2^-268, which is far under the 2^-114 the worst case needs.
    let mut window = [0u64; 7];
    for (t, slot) in window.iter_mut().enumerate() {
        let index = chunk + 5 - t as i32;
        *slot = usize::try_from(index)
            .ok()
            .and_then(|i| TWO_OVER_PI.get(i))
            .copied()
            .unwrap_or(0);
    }

    // The product, exactly. 53 bits times 448 gives 501, so eight limbs hold it.
    let mut product = [0u64; 8];
    let mut carry = 0u128;
    for t in 0..7 {
        let acc = u128::from(mantissa) * u128::from(window[t]) + carry;
        product[t] = acc as u64;
        carry = acc >> 64;
    }
    product[7] = carry as u64;

    // Applying the sub-chunk part of the exponent puts the binary point exactly at bit 384 —
    // between limb 5 and limb 6 — which is the reason for choosing the window this way.
    let mut q = [0u64; 9];
    if shift == 0 {
        q[..8].copy_from_slice(&product);
    } else {
        for t in 0..8 {
            q[t] |= product[t] << shift;
            q[t + 1] |= product[t] >> (64 - shift);
        }
    }

    // The integer part, modulo four, and the fraction in limbs 0 through 5.
    let mut n = (q[6] & 3) as i32;
    // A fraction at or above one half belongs to the next quadrant, and its remainder is
    // negative. Take the two's complement over all 384 fractional bits rather than subtracting
    // one afterwards in floating point: when the remainder is tiny — which is precisely the
    // case this whole routine exists for — the floating-point subtraction would cancel away
    // every bit that was just computed.
    let past_half = q[5] >> 63 == 1;
    if past_half {
        n += 1;
        let mut borrow = 1u64;
        for slot in q.iter_mut().take(6) {
            let (value, overflow) = (!*slot).overflowing_add(borrow);
            *slot = value;
            borrow = u64::from(overflow);
        }
    }

    // The magnitude of the remainder, as a double-double, from the top 256 fractional bits.
    // Each limb goes in as two halves because a 32-bit integer converts to `f64` exactly and a
    // 64-bit one does not.
    let (mut hi, mut lo) = (0.0, 0.0);
    for t in (2..6).rev() {
        let place = 64 * t as i32 - 384;
        let upper = (q[t] >> 32) as f64;
        let lower = (q[t] & 0xffff_ffff) as f64;
        (hi, lo) = dd_add(hi, lo, upper * pow2(place + 32));
        (hi, lo) = dd_add(hi, lo, lower * pow2(place));
    }

    // Turn the fraction of a quadrant back into radians.
    let (scaled, err) = two_product(hi, PIO2_HI);
    let tail = hi * PIO2_LO + lo * PIO2_HI + err;
    let (y_hi, y_lo) = two_sum(scaled, tail);

    // The remainder is negative when the fraction was rounded up, and the whole reduction
    // flips when `x` was negative. Both at once cancel.
    if past_half != negative {
        (if negative { -n } else { n }, -y_hi, -y_lo)
    } else {
        (if negative { -n } else { n }, y_hi, y_lo)
    }
}

/// Minimax coefficients for `sin(x)/x - 1`, from fdlibm.
const S1: f64 = -1.666_666_666_666_663_243_48e-1;
const S2: f64 = 8.333_333_333_322_489_461_24e-3;
const S3: f64 = -1.984_126_982_985_794_931_34e-4;
const S4: f64 = 2.755_731_370_707_006_767_89e-6;
const S5: f64 = -2.505_076_025_340_686_341_95e-8;
const S6: f64 = 1.589_690_995_211_550_102_21e-10;

/// `sin(x + y)` for `|x| <= pi/4`, with `y` the tail left over from argument reduction.
///
/// `tail` says whether `y` is worth the extra arithmetic. On the fast path — a small argument
/// that needed no reduction — there is no tail, and the cheaper form is not merely faster but
/// slightly more accurate, since it avoids two operations that can only add rounding.
fn sin_kernel(x: f64, y: f64, tail: bool) -> f64 {
    let z = x * x;
    let w = z * z;
    let r = S2 + z * (S3 + z * S4) + z * w * (S5 + z * S6);
    let v = z * x;
    if tail {
        x - ((z * (0.5 * y - v * r) - y) - v * S1)
    } else {
        x + v * (S1 + z * r)
    }
}

/// Minimax coefficients for `cos(x) - 1 + x²/2`, from fdlibm.
const C1: f64 = 4.166_666_666_666_660_190_37e-2;
const C2: f64 = -1.388_888_888_887_410_957_49e-3;
const C3: f64 = 2.480_158_728_947_672_941_78e-5;
const C4: f64 = -2.755_731_435_139_066_330_35e-7;
const C5: f64 = 2.087_572_321_298_174_827_9e-9;
const C6: f64 = -1.135_964_755_778_819_482_65e-11;

/// `cos(x + y)` for `|x| <= pi/4`.
///
/// The final assembly is not `1 - z/2 + z²*r`. Near `x = pi/4` the term `z/2` is about `0.3`,
/// and subtracting it from one loses a bit that `((1 - w) - hz)` — the exact remainder of that
/// same subtraction — puts straight back.
fn cos_kernel(x: f64, y: f64) -> f64 {
    let z = x * x;
    let q = z * z;
    let r = z * (C1 + z * (C2 + z * C3)) + q * q * (C4 + z * (C5 + z * C6));
    let hz = 0.5 * z;
    let w = 1.0 - hz;
    w + (((1.0 - w) - hz) + (z * r - x * y))
}

/// Minimax coefficients for `tan(x)/x - 1`, from fdlibm.
const T: [f64; 13] = [
    3.333_333_333_333_340_919_86e-1,
    1.333_333_333_332_012_426_99e-1,
    5.396_825_397_622_605_213_77e-2,
    2.186_948_829_485_954_245_99e-2,
    8.863_239_823_599_300_057_37e-3,
    3.592_079_107_591_312_353_56e-3,
    1.456_209_454_325_290_255_16e-3,
    5.880_412_408_202_640_968_74e-4,
    2.464_631_348_184_699_068_12e-4,
    7.817_944_429_395_570_923e-5,
    7.140_724_913_826_081_903_05e-5,
    -1.855_863_748_552_754_566_54e-5,
    2.590_730_518_636_337_128_84e-5,
];
/// `pi/4`, split so the tangent's own secondary reduction stays exact.
const PIO4_HI: f64 = core::f64::consts::FRAC_PI_4;
/// The tail of [`PIO4_HI`].
const PIO4_LO: f64 = 3.061_616_997_868_383_017_93e-17;

/// `tan(x + y)` for `|x| <= pi/4`, or its negated reciprocal when `reciprocal` is set.
///
/// The reciprocal branch is not `-1.0 / tan(x)`. That division carries two units in the last
/// place of error into a result that is often large, so the quotient is refined by one
/// Newton step on a deliberately truncated estimate instead.
#[allow(clippy::indexing_slicing)]
fn tan_kernel(mut x: f64, mut y: f64, reciprocal: bool) -> f64 {
    let hx = (x.to_bits() >> 32) as u32;
    // Above about 0.6744 the polynomial's accuracy falls off, so fold onto pi/4 - x first and
    // recover the tangent from the identity afterwards.
    let folded = hx & 0x7fff_ffff >= 0x3fe5_9428;
    let mut negative = false;
    if folded {
        negative = hx >> 31 == 1;
        if negative {
            x = -x;
            y = -y;
        }
        x = (PIO4_HI - x) + (PIO4_LO - y);
        y = 0.0;
    }

    let z = x * x;
    let w = z * z;
    let odd = T[1] + w * (T[3] + w * (T[5] + w * (T[7] + w * (T[9] + w * T[11]))));
    let even = z * (T[2] + w * (T[4] + w * (T[6] + w * (T[8] + w * (T[10] + w * T[12])))));
    let s = z * x;
    let r = y + z * (s * (odd + even) + y) + s * T[0];
    let value = x + r;

    if folded {
        let sign = if reciprocal { -1.0 } else { 1.0 };
        let v = sign - 2.0 * (x + (r - value * value / (value + sign)));
        return if negative { -v } else { v };
    }
    if !reciprocal {
        return value;
    }
    // Truncating to 21 significant bits makes `1 + t*z` exact, which is what lets one Newton
    // step reach full precision rather than merely most of it.
    let z = f64::from_bits(value.to_bits() & 0xffff_ffff_0000_0000);
    let v = r - (z - x);
    let a = -1.0 / value;
    let t = f64::from_bits(a.to_bits() & 0xffff_ffff_0000_0000);
    t + a * ((1.0 + t * z) + t * v)
}

/// The sine of `x`, in radians.
///
/// Accurate for every finite argument, including ones far larger than `pi` — see the module
/// documentation for why that is not the free property it sounds like.
pub fn sin(x: f64) -> f64 {
    let ix = (x.to_bits() >> 32) as u32 & 0x7fff_ffff;
    if ix <= 0x3fe9_21fb {
        // |x| < pi/4, so there is no quadrant to find.
        if ix < 0x3e50_0000 {
            // |x| < 2^-26: sin(x) and x agree to every bit.
            return x;
        }
        return sin_kernel(x, 0.0, false);
    }
    if ix >= 0x7ff0_0000 {
        // Infinity has no sine, and a NaN stays one.
        return crate::invalid(x);
    }
    let (n, y, tail) = rem_pio2(x);
    match n & 3 {
        0 => sin_kernel(y, tail, true),
        1 => cos_kernel(y, tail),
        2 => -sin_kernel(y, tail, true),
        _ => -cos_kernel(y, tail),
    }
}

/// The cosine of `x`, in radians.
pub fn cos(x: f64) -> f64 {
    let ix = (x.to_bits() >> 32) as u32 & 0x7fff_ffff;
    if ix <= 0x3fe9_21fb {
        if ix < 0x3e46_a09e {
            // |x| < 2^-27: cos(x) rounds to exactly 1.
            return 1.0;
        }
        return cos_kernel(x, 0.0);
    }
    if ix >= 0x7ff0_0000 {
        return crate::invalid(x);
    }
    let (n, y, tail) = rem_pio2(x);
    match n & 3 {
        0 => cos_kernel(y, tail),
        1 => -sin_kernel(y, tail, true),
        2 => -cos_kernel(y, tail),
        _ => sin_kernel(y, tail, true),
    }
}

/// The tangent of `x`, in radians.
pub fn tan(x: f64) -> f64 {
    let ix = (x.to_bits() >> 32) as u32 & 0x7fff_ffff;
    if ix <= 0x3fe9_21fb {
        if ix < 0x3e40_0000 {
            // |x| < 2^-27: tan(x) and x agree to every bit.
            return x;
        }
        return tan_kernel(x, 0.0, false);
    }
    if ix >= 0x7ff0_0000 {
        return crate::invalid(x);
    }
    let (n, y, tail) = rem_pio2(x);
    // Odd quadrants are where tangent runs through its pole, and the identity there is the
    // negated reciprocal rather than the value itself.
    tan_kernel(y, tail, n & 1 == 1)
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
        assert_eq!(sin(0.0), 0.0);
        assert!(sin(-0.0).is_sign_negative());
        assert_eq!(cos(0.0), 1.0);
        assert_eq!(tan(0.0), 0.0);
        assert!(sin(f64::INFINITY).is_nan());
        assert!(cos(f64::INFINITY).is_nan());
        assert!(tan(f64::NEG_INFINITY).is_nan());
        assert!(sin(f64::NAN).is_nan());
        assert!(cos(f64::NAN).is_nan());
        assert!(tan(f64::NAN).is_nan());
    }

    #[test]
    fn the_functions_have_the_symmetries_they_are_supposed_to() {
        let mut state = 0x1234_5678_9abc_def0_u64;
        let mut next = || {
            state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        };
        for _ in 0..20_000 {
            let x = (next() as f64 / u64::MAX as f64) * 2000.0 - 1000.0;
            // Odd, even, odd — exactly, not approximately, because the reduction of -x is the
            // negation of the reduction of x by construction.
            assert_eq!(sin(-x), -sin(x), "sin at {x}");
            assert_eq!(cos(-x).to_bits(), cos(x).to_bits(), "cos at {x}");
            assert_eq!(tan(-x), -tan(x), "tan at {x}");
            // The Pythagorean identity, to within what f64 can hold.
            let (s, c) = (sin(x), cos(x));
            assert!((s * s + c * c - 1.0).abs() < 1e-15, "identity at {x}");
        }
    }

    /// The property the exact reduction is for. A reduction that runs out of precision above
    /// some threshold does not fail loudly — it returns a plausible number that is wrong, so
    /// the check has to be against a value known independently.
    #[test]
    fn enormous_arguments_reduce_correctly_rather_than_plausibly() {
        // Distance in representable numbers, so the comparison means the same thing whether
        // the answer is near one or near zero.
        let gap = |a: f64, b: f64| {
            let key = |x: f64| {
                let bits = x.to_bits();
                let magnitude = (bits & !(1u64 << 63)) as i64;
                if bits >> 63 == 1 {
                    -magnitude
                } else {
                    magnitude
                }
            };
            (key(a) - key(b)).abs()
        };

        // sin(2^n) for every n, against the platform libm, which also reduces exactly.
        for n in 0..1000 {
            let x = crate::scalbn(1.0, n);
            let d = gap(sin(x), x.sin());
            assert!(d <= 2, "sin(2^{n}) is {d} off: {} vs {}", sin(x), x.sin());
        }

        // Just below a pole of the tangent, where the answer is enormous and its accuracy
        // depends entirely on the reduced argument being right.
        let x = 1e300_f64;
        assert!(
            gap(tan(x), x.tan()) <= 2,
            "tan(1e300): {} vs {}",
            tan(x),
            x.tan()
        );
    }

    /// The worst case for double-precision argument reduction, against the true answer.
    ///
    /// This one is not checked against the platform, because **the platform gets it wrong**.
    /// For `x = 6381956970095103 * 2^797`, the remainder `x mod (pi/2)` is `4.687e-19` — about
    /// `2^-61` — so a reduction must carry more than 110 bits of `2/pi` for the reduced value
    /// to have a single correct bit. The right answers below were established three
    /// independent ways: by exact integer arithmetic over a 3000-bit `pi` computed from
    /// Machin's formula, by V8, and by this crate, all agreeing to the last bit.
    ///
    /// The Windows platform libm this was written against returns **-0.2227** for the sine,
    /// which is not near 1 and not near anything else either. It is the failure this whole
    /// module is arranged to avoid, and it is not hypothetical: it ships.
    #[test]
    fn the_worst_case_in_the_format_comes_out_right_even_though_the_platform_gets_it_wrong() {
        let x = 6_381_956_970_095_103.0_f64 * crate::scalbn(1.0, 797);
        // The remainder is 4.687e-19, so the argument is a hair past an odd multiple of pi/2:
        // the sine is one to every bit f64 has, and the cosine is the remainder, negated.
        assert_eq!(sin(x), 1.0);
        assert_eq!(cos(x).to_bits(), 0xbc21_4ae7_2e6b_a22f);
        // And the tangent, which is the ratio of the two and therefore enormous: about
        // -2.13e18. This one is checked against V8, bit for bit.
        assert_eq!(tan(x).to_bits(), 0xc3bd_9ba9_a797_5636);
    }

    /// Big-integer arithmetic, so that [`TWO_OVER_PI`] can be re-derived rather than trusted.
    ///
    /// Limb indices are the algorithm here, as they are in the reduction itself, and every
    /// loop is bounded by the length of the vector it walks.
    #[allow(clippy::indexing_slicing, clippy::needless_range_loop)]
    mod big {
        use std::cmp::Ordering;

        pub type Big = Vec<u32>;

        fn trim(a: &mut Big) {
            while a.len() > 1 && a.last() == Some(&0) {
                a.pop();
            }
        }
        pub fn is_zero(a: &Big) -> bool {
            a.iter().all(|&x| x == 0)
        }
        pub fn cmp(a: &Big, b: &Big) -> Ordering {
            for i in (0..a.len().max(b.len())).rev() {
                let (x, y) = (*a.get(i).unwrap_or(&0), *b.get(i).unwrap_or(&0));
                if x != y {
                    return x.cmp(&y);
                }
            }
            Ordering::Equal
        }
        pub fn add_assign(a: &mut Big, b: &Big) {
            if a.len() < b.len() {
                a.resize(b.len(), 0);
            }
            let mut carry = 0u64;
            for i in 0..a.len() {
                let t = u64::from(a[i]) + u64::from(*b.get(i).unwrap_or(&0)) + carry;
                a[i] = t as u32;
                carry = t >> 32;
            }
            if carry != 0 {
                a.push(carry as u32);
            }
        }
        pub fn sub_assign(a: &mut Big, b: &Big) {
            let mut borrow = 0i64;
            for i in 0..a.len() {
                let t = i64::from(a[i]) - i64::from(*b.get(i).unwrap_or(&0)) - borrow;
                if t < 0 {
                    a[i] = (t + (1i64 << 32)) as u32;
                    borrow = 1;
                } else {
                    a[i] = t as u32;
                    borrow = 0;
                }
            }
            assert_eq!(borrow, 0, "subtraction went negative");
            trim(a);
        }
        pub fn mul_small(a: &Big, m: u64) -> Big {
            let mut out = Vec::with_capacity(a.len() + 2);
            let mut carry = 0u64;
            for &limb in a {
                let t = u64::from(limb) * m + carry;
                out.push(t as u32);
                carry = t >> 32;
            }
            while carry != 0 {
                out.push(carry as u32);
                carry >>= 32;
            }
            trim(&mut out);
            out
        }
        pub fn div_small(a: &Big, d: u64) -> Big {
            let mut out = vec![0u32; a.len()];
            let mut rem = 0u64;
            for i in (0..a.len()).rev() {
                let cur = (rem << 32) | u64::from(a[i]);
                out[i] = (cur / d) as u32;
                rem = cur % d;
            }
            trim(&mut out);
            out
        }
        pub fn one_shl(bits: usize) -> Big {
            let mut out = vec![0u32; bits / 32 + 1];
            out[bits / 32] = 1u32 << (bits % 32);
            out
        }
        pub fn bit(a: &Big, i: usize) -> bool {
            a.get(i / 32).is_some_and(|w| (w >> (i % 32)) & 1 == 1)
        }
        pub fn set_bit(a: &mut Big, i: usize) {
            if a.len() <= i / 32 {
                a.resize(i / 32 + 1, 0);
            }
            a[i / 32] |= 1u32 << (i % 32);
        }
        pub fn shl1(a: &mut Big) {
            let mut carry = 0u32;
            for limb in a.iter_mut() {
                let next = *limb >> 31;
                *limb = (*limb << 1) | carry;
                carry = next;
            }
            if carry != 0 {
                a.push(carry);
            }
        }
    }

    /// Recomputes [`TWO_OVER_PI`] from Machin's formula and checks every limb.
    ///
    /// A single wrong bit in that table would not announce itself. Small arguments would stay
    /// correct — they barely touch it — and large ones would return numbers of the right
    /// magnitude that are simply wrong. This is the test that makes such a bug impossible.
    #[test]
    fn the_table_is_two_over_pi() {
        use big::*;
        /// Bits of working precision for pi. Far more than the table needs, so the error in
        /// the division is nowhere near the last limb.
        const PRECISION: usize = 2600;

        // atan(1/n) * 2^PRECISION, by the alternating series.
        let atan_inv = |n: u64| -> Big {
            let mut power = div_small(&one_shl(PRECISION), n);
            let mut sum: Big = vec![0];
            let mut k = 0u64;
            loop {
                let t = div_small(&power, 2 * k + 1);
                if is_zero(&t) {
                    break;
                }
                if k % 2 == 0 {
                    add_assign(&mut sum, &t);
                } else {
                    sub_assign(&mut sum, &t);
                }
                power = div_small(&power, n * n);
                if is_zero(&power) {
                    break;
                }
                k += 1;
            }
            sum
        };

        // pi = 16*atan(1/5) - 4*atan(1/239).
        let mut pi = mul_small(&atan_inv(5), 16);
        sub_assign(&mut pi, &mul_small(&atan_inv(239), 4));

        // Independent check on pi itself before it is used: its leading 64 bits are a constant
        // published in RFC 3526, and arrived at here by an entirely different route.
        let mut leading = 0u64;
        for i in 0..64 {
            leading = (leading << 1) | u64::from(bit(&pi, PRECISION + 1 - i));
        }
        assert_eq!(leading, 0xc90f_daa2_2168_c234, "Machin did not produce pi");

        // The table is the leading bits of 2/pi, so divide 2^(width+1+PRECISION) by pi scaled.
        let width = 64 * TWO_OVER_PI.len();
        let top = width + 1 + PRECISION;
        let (mut quotient, mut remainder): (Big, Big) = (vec![0], vec![0]);
        for i in (0..=top).rev() {
            shl1(&mut remainder);
            if i == top {
                add_assign(&mut remainder, &vec![1]);
            }
            if cmp(&remainder, &pi) != std::cmp::Ordering::Less {
                sub_assign(&mut remainder, &pi);
                set_bit(&mut quotient, i);
            }
        }
        assert!(!bit(&quotient, width), "2/pi should be below one");

        for (j, &want) in TWO_OVER_PI.iter().enumerate() {
            let mut limb = 0u64;
            for b in 0..64 {
                limb = (limb << 1) | u64::from(bit(&quotient, width - (64 * j + b) - 1));
            }
            assert_eq!(limb, want, "2/pi chunk {j}");
        }
    }

    /// The constants that were split by hand, checked against their unsplit forms.
    #[test]
    fn the_split_constants_reassemble() {
        assert_eq!(PIO2_HI.to_bits(), std::f64::consts::FRAC_PI_2.to_bits());
        assert_eq!(PIO4_HI.to_bits(), std::f64::consts::FRAC_PI_4.to_bits());
        // Each tail is what its head is missing, so head plus tail must round back to head.
        assert_eq!((PIO2_HI + PIO2_LO).to_bits(), PIO2_HI.to_bits());
        assert_eq!((PIO4_HI + PIO4_LO).to_bits(), PIO4_HI.to_bits());
        // And the two tails must scale together, since they come from the same pi.
        assert_eq!(PIO2_LO, 2.0 * PIO4_LO);
    }
}
