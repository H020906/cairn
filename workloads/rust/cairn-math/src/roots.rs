//! Roots and lengths.

use crate::two_product;

/// The square root of `x`.
///
/// Here for company rather than because it needed writing: `sqrt` is a single WebAssembly
/// instruction, `f64.sqrt`, and IEEE-754 requires it to be correctly rounded — so unlike every
/// other function in this crate, every engine already agrees about it. It is exported so that a
/// workload can take all of its arithmetic from one place.
#[inline]
pub fn sqrt(x: f64) -> f64 {
    x.sqrt()
}

/// Exponent bias for the initial cube-root estimate, from fdlibm. Encodes
/// `(1023 - 1023/3 - 0.03306235651) * 2^20`.
const B1: u32 = 715_094_163;
/// The same bias for an argument that had to be scaled out of the subnormal range first.
const B2: u32 = 696_219_795;

/// Coefficients of a degree-4 approximation to `1/cbrt(r)` on the reduced range, good to
/// `2^-23.5`, from fdlibm.
const P0: f64 = 1.875_951_824_271_770_096_43e0;
const P1: f64 = -1.884_979_795_433_771_698_75e0;
const P2: f64 = 1.621_429_720_105_354_466_14e0;
const P3: f64 = -7.583_979_347_787_660_474_37e-1;
const P4: f64 = 1.459_961_928_866_124_469_82e-1;

/// The cube root of `x`, defined for negative arguments as well as positive ones.
///
/// `pow(x, 1.0/3.0)` is not this function twice over: it is NaN for every negative `x`, because
/// `1.0/3.0` is not exactly a third and a non-integral power of a negative number has no real
/// value. It is also less accurate for positive ones, since a third cannot be represented and
/// the error in the exponent is multiplied by `ln(x)`.
///
/// The first estimate is made by dividing the exponent field by three — five correct bits for
/// the price of an integer division — then refined by a polynomial to 23 bits and a single
/// Newton step to full precision.
pub fn cbrt(x: f64) -> f64 {
    let mut bits = x.to_bits();
    let hx = (bits >> 32) as u32 & 0x7fff_ffff;

    if hx >= 0x7ff0_0000 {
        // Infinity and NaN are their own cube roots.
        return x + x;
    }

    let hx = if hx < 0x0010_0000 {
        // Zero or subnormal. Scale up by 2^54 so the exponent arithmetic has something to work
        // with, and compensate with the other bias constant.
        let scaled = x * f64::from_bits(0x4350_0000_0000_0000);
        bits = scaled.to_bits();
        let hx = (bits >> 32) as u32 & 0x7fff_ffff;
        if hx == 0 {
            // cbrt(±0) is ±0, sign included.
            return x;
        }
        hx / 3 + B2
    } else {
        hx / 3 + B1
    };

    // Keep the sign, replace everything else with the estimated exponent.
    let mut t = f64::from_bits((bits & (1 << 63)) | (u64::from(hx) << 32));

    // Refine to 23 bits. `t*cbrt(x/t³)` with the inner cube root taken from the polynomial.
    let r = (t * t) * (t / x);
    t *= (P0 + r * (P1 + r * P2)) + ((r * r) * r) * (P3 + r * P4);

    // Round the estimate to 23 bits, away from zero. This is what makes `t*t` below exact,
    // which in turn is what makes the Newton step land within two thirds of a unit in the last
    // place rather than merely close.
    t = f64::from_bits((t.to_bits() + 0x8000_0000) & 0xffff_ffff_c000_0000);

    // One Newton step. Every subtraction here is exact by construction: `t*t` because `t` has
    // 23 bits, `r - t` because they agree to 23 bits, and `t + t` always.
    let s = t * t;
    let r = x / s;
    let w = t + t;
    let r = (r - t) / (w + r);
    t + t * r
}

/// The length of the hypotenuse with legs `x` and `y`, without the overflow that
/// `sqrt(x*x + y*y)` would suffer.
///
/// `sqrt(x*x + y*y)` is wrong at both ends of the range and in the middle. At the top, `x*x`
/// overflows to infinity for any `|x|` above about `1.3e154`, so `hypot(1e200, 1e200)` returns
/// infinity when the answer is an ordinary `1.41e200`. At the bottom, `x*x` flushes to zero
/// below `1.5e-162` and the answer collapses to zero. In between, squaring and adding rounds
/// twice before the square root rounds a third time.
///
/// This scales the operands into safe territory when needed, and squares them exactly — as a
/// pair of doubles apiece — so the sum handed to `sqrt` has more precision than a `f64` holds
/// and only one rounding remains.
pub fn hypot(x: f64, y: f64) -> f64 {
    let (mut bx, mut by) = (x.to_bits() & !(1 << 63), y.to_bits() & !(1 << 63));
    if bx < by {
        core::mem::swap(&mut bx, &mut by);
    }
    let (ex, ey) = ((bx >> 52) as i32, (by >> 52) as i32);
    let (mut x, mut y) = (f64::from_bits(bx), f64::from_bits(by));

    // NaN in the smaller slot wins; an infinity in the larger one does. Ordering the two checks
    // this way is what makes `hypot(inf, nan)` infinite, which is what the standard asks for.
    if ey == 0x7ff {
        return y;
    }
    if ex == 0x7ff || by == 0 {
        return x;
    }
    // More than 64 binary orders of magnitude apart: the smaller leg cannot reach the answer's
    // last bit.
    if ex - ey > 64 {
        return x + y;
    }

    // Bring both into the range where squaring neither overflows nor underflows, and remember
    // the factor to undo afterwards.
    let mut scale = 1.0;
    if ex > 0x3ff + 510 {
        scale = f64::from_bits(0x6bb0_0000_0000_0000);
        x *= f64::from_bits(0x1430_0000_0000_0000);
        y *= f64::from_bits(0x1430_0000_0000_0000);
    } else if ey < 0x3ff - 450 {
        scale = f64::from_bits(0x1430_0000_0000_0000);
        x *= f64::from_bits(0x6bb0_0000_0000_0000);
        y *= f64::from_bits(0x6bb0_0000_0000_0000);
    }

    // Both squares, exactly, as head-and-tail pairs. Adding the two tails first keeps them
    // from being lost against the heads.
    let (hx, lx) = two_product(x, x);
    let (hy, ly) = two_product(y, y);
    scale * sqrt(ly + lx + hy + hx)
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
    fn cbrt_handles_the_signs_and_the_ends() {
        assert_eq!(cbrt(0.0), 0.0);
        assert!(cbrt(-0.0).is_sign_negative());
        assert_eq!(cbrt(1.0), 1.0);
        assert_eq!(cbrt(-1.0), -1.0);
        assert_eq!(cbrt(8.0), 2.0);
        assert_eq!(cbrt(-27.0), -3.0);
        assert_eq!(cbrt(f64::INFINITY), f64::INFINITY);
        assert_eq!(cbrt(f64::NEG_INFINITY), f64::NEG_INFINITY);
        assert!(cbrt(f64::NAN).is_nan());
        // The subnormal path, which needs the other bias constant.
        let tiny = f64::from_bits(1);
        assert!((cbrt(tiny) - tiny.cbrt()).abs() <= f64::EPSILON * cbrt(tiny));
        // Negative arguments, which is the whole reason not to write pow(x, 1.0/3.0).
        assert!(crate::pow(-27.0, 1.0 / 3.0).is_nan());
    }

    /// Every case in the doc comment, checked rather than asserted in prose.
    #[test]
    fn hypot_survives_the_three_places_the_obvious_formula_fails() {
        // Overflow: x*x is infinite, the answer is not.
        let big = 1e200_f64;
        assert!((big * big + big * big).is_infinite());
        assert!((hypot(big, big) - big * std::f64::consts::SQRT_2).abs() < 1e185);

        // Underflow: x*x is zero, the answer is not.
        let small = 1e-200;
        assert_eq!(small * small + small * small, 0.0);
        assert!((hypot(small, small) - small * std::f64::consts::SQRT_2).abs() < 1e-215);

        // Ordinary values, exactly.
        assert_eq!(hypot(3.0, 4.0), 5.0);
        assert_eq!(hypot(-3.0, -4.0), 5.0);
        assert_eq!(hypot(0.0, 0.0), 0.0);
        assert_eq!(hypot(0.0, -5.0), 5.0);

        // Infinity beats NaN, in both orders.
        assert_eq!(hypot(f64::INFINITY, f64::NAN), f64::INFINITY);
        assert_eq!(hypot(f64::NAN, f64::INFINITY), f64::INFINITY);
        assert!(hypot(f64::NAN, 1.0).is_nan());
        assert_eq!(hypot(f64::NEG_INFINITY, 1.0), f64::INFINITY);
    }
}
