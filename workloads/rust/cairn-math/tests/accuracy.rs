//! How close each function is to the platform's own math library.
//!
//! # What this measures, and what it does not
//!
//! This is a **quality** measurement, not the correctness property Cairn depends on. It cannot
//! be one, because the reference it compares against is not correct either: the platform libm
//! is itself accurate to a unit or two in the last place rather than perfectly rounded, and the
//! crate documentation's opening table is exactly a measurement of two platforms disagreeing
//! with each other. A gap of two here means the two implementations differ by two, and says
//! nothing about which of them is nearer the true value.
//!
//! What it is good for is catching a mistake. A wrong polynomial coefficient, a mis-split
//! constant, a reduction that runs out of precision — none of those produce a two-unit
//! disagreement. They produce a hundred, or a thousand, or a result of the wrong magnitude
//! entirely. The ceilings asserted below are set just above what was measured, so a change that
//! damages a function fails here rather than being discovered by whoever was relying on it.
//!
//! The property that actually matters — that four independent engines produce **identical**
//! bits — is tested in `runtime/tests/differential.rs`, where every other such claim in this
//! project is tested.
//!
//! Run with `--nocapture` to see the table.

/// Maps a `f64` onto a signed integer that increases with the value, so that the distance
/// between two of them counts representable numbers rather than magnitudes.
fn key(x: f64) -> i64 {
    let bits = x.to_bits();
    let magnitude = (bits & !(1u64 << 63)) as i64;
    if bits >> 63 == 1 {
        -magnitude
    } else {
        magnitude
    }
}

/// How many representable numbers apart two results are.
///
/// Returns `None` when there is nothing to compare — both NaN, or both the same infinity —
/// and a deliberately enormous number when only one of them is.
fn gap(mine: f64, theirs: f64) -> Option<i64> {
    if mine.is_nan() || theirs.is_nan() {
        return if mine.is_nan() == theirs.is_nan() {
            None
        } else {
            Some(i64::MAX)
        };
    }
    if mine.is_infinite() || theirs.is_infinite() {
        return if mine.to_bits() == theirs.to_bits() {
            None
        } else {
            Some(i64::MAX)
        };
    }
    Some((key(mine) - key(theirs)).abs())
}

/// A deterministic stream of bits, so a failure names an input that can be reproduced.
struct Bits(u64);

impl Bits {
    fn new(seed: u64) -> Self {
        Self(seed)
    }
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    /// Uniform on `[low, high]`.
    fn uniform(&mut self, low: f64, high: f64) -> f64 {
        low + (self.next() as f64 / u64::MAX as f64) * (high - low)
    }
    /// Log-uniform and positive, spanning the exponents in `[low, high]`. This is the right
    /// shape for the logarithms and for `pow`'s base, where the interesting variation is in
    /// the exponent rather than the mantissa.
    fn scaled(&mut self, low: i32, high: i32) -> f64 {
        let exponent = low + (self.next() % (high - low + 1) as u64) as i32;
        let mantissa = self.next() & ((1 << 52) - 1);
        f64::from_bits((((1023 + exponent) as u64) << 52) | mantissa)
    }
}

/// One measured function.
struct Case {
    name: &'static str,
    samples: usize,
    worst: i64,
    at: String,
}

impl Case {
    /// Runs `count` samples of a one-argument function against a reference.
    fn one(
        name: &'static str,
        count: usize,
        seed: u64,
        mut input: impl FnMut(&mut Bits) -> f64,
        mine: impl Fn(f64) -> f64,
        theirs: impl Fn(f64) -> f64,
    ) -> Self {
        let mut bits = Bits::new(seed);
        let (mut worst, mut at) = (0, String::new());
        for _ in 0..count {
            let x = input(&mut bits);
            if let Some(d) = gap(mine(x), theirs(x)) {
                if d > worst {
                    worst = d;
                    at = format!("{x:e} -> {:e} vs {:e}", mine(x), theirs(x));
                }
            }
        }
        Self {
            name,
            samples: count,
            worst,
            at,
        }
    }

    /// The same, for a two-argument function.
    fn two(
        name: &'static str,
        count: usize,
        seed: u64,
        mut input: impl FnMut(&mut Bits) -> (f64, f64),
        mine: impl Fn(f64, f64) -> f64,
        theirs: impl Fn(f64, f64) -> f64,
    ) -> Self {
        let mut bits = Bits::new(seed);
        let (mut worst, mut at) = (0, String::new());
        for _ in 0..count {
            let (x, y) = input(&mut bits);
            if let Some(d) = gap(mine(x, y), theirs(x, y)) {
                if d > worst {
                    worst = d;
                    at = format!("({x:e}, {y:e}) -> {:e} vs {:e}", mine(x, y), theirs(x, y));
                }
            }
        }
        Self {
            name,
            samples: count,
            worst,
            at,
        }
    }
}

/// How far each function is allowed to be from the platform's, in representable steps.
///
/// These are not aspirations. Each is the measured worst case, rounded up to leave room for a
/// platform whose libm differs slightly from the one this was measured on — the whole premise
/// of the crate is that such platforms exist. A number that has to be *raised* to make this
/// test pass is a regression, and should be treated as one.
const CEILING: i64 = 4;

#[test]
fn every_function_agrees_with_the_platform_to_within_a_few_units_in_the_last_place() {
    const N: usize = 200_000;
    let cases = vec![
        Case::one(
            "exp",
            N,
            1,
            |b| b.uniform(-700.0, 700.0),
            cairn_math::exp,
            f64::exp,
        ),
        Case::one(
            "exp2",
            N,
            2,
            |b| b.uniform(-1000.0, 1000.0),
            cairn_math::exp2,
            f64::exp2,
        ),
        Case::one(
            "expm1",
            N,
            3,
            |b| b.uniform(-40.0, 40.0),
            cairn_math::expm1,
            f64::exp_m1,
        ),
        Case::one(
            "expm1 (tiny)",
            N,
            4,
            |b| b.uniform(-1e-8, 1e-8),
            cairn_math::expm1,
            f64::exp_m1,
        ),
        Case::one(
            "ln",
            N,
            5,
            |b| b.scaled(-1020, 1020),
            cairn_math::ln,
            f64::ln,
        ),
        Case::one(
            "log2",
            N,
            6,
            |b| b.scaled(-1020, 1020),
            cairn_math::log2,
            f64::log2,
        ),
        Case::one(
            "log10",
            N,
            7,
            |b| b.scaled(-1020, 1020),
            cairn_math::log10,
            f64::log10,
        ),
        Case::one(
            "ln_1p",
            N,
            8,
            |b| b.uniform(-0.9, 20.0),
            cairn_math::ln_1p,
            f64::ln_1p,
        ),
        Case::one(
            "ln_1p (tiny)",
            N,
            9,
            |b| b.uniform(-1e-8, 1e-8),
            cairn_math::ln_1p,
            f64::ln_1p,
        ),
        Case::one(
            "sin",
            N,
            10,
            |b| b.uniform(-1000.0, 1000.0),
            cairn_math::sin,
            f64::sin,
        ),
        Case::one(
            "cos",
            N,
            11,
            |b| b.uniform(-1000.0, 1000.0),
            cairn_math::cos,
            f64::cos,
        ),
        Case::one(
            "tan",
            N,
            12,
            |b| b.uniform(-1000.0, 1000.0),
            cairn_math::tan,
            f64::tan,
        ),
        // The reduction's real test: arguments where a limited-precision reduction returns
        // noise. Every one of these is far past 2^20.
        Case::one(
            "sin (enormous)",
            N,
            13,
            |b| b.scaled(100, 1020),
            cairn_math::sin,
            f64::sin,
        ),
        Case::one(
            "cos (enormous)",
            N,
            14,
            |b| b.scaled(100, 1020),
            cairn_math::cos,
            f64::cos,
        ),
        Case::one(
            "tan (enormous)",
            N,
            15,
            |b| b.scaled(100, 1020),
            cairn_math::tan,
            f64::tan,
        ),
        Case::one(
            "asin",
            N,
            16,
            |b| b.uniform(-1.0, 1.0),
            cairn_math::asin,
            f64::asin,
        ),
        Case::one(
            "acos",
            N,
            17,
            |b| b.uniform(-1.0, 1.0),
            cairn_math::acos,
            f64::acos,
        ),
        Case::one(
            "atan",
            N,
            18,
            |b| b.uniform(-100.0, 100.0),
            cairn_math::atan,
            f64::atan,
        ),
        Case::one(
            "sinh",
            N,
            19,
            |b| b.uniform(-30.0, 30.0),
            cairn_math::sinh,
            f64::sinh,
        ),
        Case::one(
            "cosh",
            N,
            20,
            |b| b.uniform(-30.0, 30.0),
            cairn_math::cosh,
            f64::cosh,
        ),
        Case::one(
            "tanh",
            N,
            21,
            |b| b.uniform(-30.0, 30.0),
            cairn_math::tanh,
            f64::tanh,
        ),
        Case::one(
            "cbrt",
            N,
            22,
            |b| b.scaled(-1020, 1020),
            cairn_math::cbrt,
            f64::cbrt,
        ),
        Case::two(
            "atan2",
            N,
            23,
            |b| (b.uniform(-100.0, 100.0), b.uniform(-100.0, 100.0)),
            cairn_math::atan2,
            f64::atan2,
        ),
        Case::two(
            "hypot",
            N,
            24,
            |b| (b.scaled(-300, 300), b.scaled(-300, 300)),
            cairn_math::hypot,
            f64::hypot,
        ),
        Case::two(
            "pow",
            N,
            25,
            |b| (b.scaled(-40, 40), b.uniform(-50.0, 50.0)),
            cairn_math::pow,
            f64::powf,
        ),
        Case::two(
            "pow (small y)",
            N,
            26,
            |b| (b.scaled(-1020, 1020), b.uniform(-2.0, 2.0)),
            cairn_math::pow,
            f64::powf,
        ),
    ];

    println!("\nAgreement with the platform libm, worst of {N} samples per function.");
    println!("A gap of n means the two implementations differ by n representable numbers.\n");
    println!("{:<16}{:>10}   where", "function", "worst gap");
    for case in &cases {
        let worst = if case.worst == i64::MAX {
            "MISMATCH".to_owned()
        } else {
            case.worst.to_string()
        };
        println!("{:<16}{worst:>10}   {}", case.name, case.at);
    }
    println!();

    let bad: Vec<_> = cases.iter().filter(|c| c.worst > CEILING).collect();
    assert!(
        bad.is_empty(),
        "beyond {CEILING} units in the last place: {}\n\n\
         Before assuming this crate is at fault, check which side moved. The reference here is \
         not authoritative, and there is a worked example of it being outright wrong: for \
         `x = 6381956970095103 * 2^797`, the platform's `sin` returns -0.2227 where the answer \
         is 1.0. A gap in the hundreds or thousands on the trigonometric rows, on a platform \
         whose libm reduces arguments in fewer than 110 bits of 2/pi, is that bug and not this \
         one — `trig.rs` has the case with values established three independent ways.",
        bad.iter()
            .map(|c| format!("{} at {} ({} samples)", c.name, c.worst, c.samples))
            .collect::<Vec<_>>()
            .join("; ")
    );
}
