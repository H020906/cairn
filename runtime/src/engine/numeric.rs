//! The numeric instructions.
//!
//! Pure stack transformations, separated from the machine so they can be tested without one.
//! This is where transcription risk lives — a hundred and fifty near-identical operations,
//! several of which have corner cases that fail silently rather than loudly — so the module is
//! deliberately small in surface and heavy in tests.
//!
//! # Corner cases that do not announce themselves
//!
//! Each of these is a place where a plausible implementation is wrong in a way no ordinary
//! test would catch, and where being wrong means two workers disagree:
//!
//! - **`div_s` traps on `INT_MIN / -1`**, because the true quotient is not representable.
//!   `rem_s` on the same operands does **not** trap; it yields zero.
//! - **Shift counts are taken modulo the width.** `i32.shl` by 32 is a shift by 0, not zero.
//! - **`min` and `max` are not `if a < b`.** Either operand being NaN gives NaN, and the sign
//!   of zero is directed: `min(+0, -0)` is `-0` and `max(+0, -0)` is `+0`.
//! - **`nearest` rounds halves to even**, not away from zero. `nearest(2.5)` is `2`.
//! - **`trunc` traps on NaN and on out-of-range**, while `trunc_sat` clamps instead. Rust's
//!   `as` cast already saturates, so the trapping forms need their range check written out.
//!
//! # Floating point is read from and written back as bits
//!
//! Values arrive as [`Value::F32`]/[`Value::F64`] holding raw bit patterns; operations widen
//! them to `f32`/`f64`, compute, and store the result's bits back. Nothing keeps a float in a
//! comparison outside the operations that are defined as comparisons. See [`crate::state`].

use wasmparser::Operator;

use crate::state::Value;

/// A trap: execution stops here, deterministically, on every machine.
///
/// Traps are part of a work unit's observable result. Two workers must agree not only on the
/// answer to a computation but on whether it trapped and where, so this enum is a consensus
/// surface and its variants must not be merged for convenience.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trap {
    /// `unreachable` was executed.
    Unreachable,
    /// An integer division or remainder by zero.
    DivideByZero,
    /// A signed division whose result is not representable: `INT_MIN / -1`.
    IntegerOverflow,
    /// A float-to-integer conversion of a NaN or of a value outside the target range.
    BadConversion,
    /// A memory access outside the addressable region.
    MemoryOutOfBounds,
    /// A table access outside the table.
    TableOutOfBounds,
    /// An indirect call to an empty table slot.
    UninitializedElement,
    /// An indirect call whose target has a different signature.
    SignatureMismatch,
    /// The call stack exceeded its limit.
    CallStackExhausted,
    /// The work unit ran out of fuel.
    OutOfFuel,
    /// The operand stack did not hold what an instruction required.
    ///
    /// Unreachable for a validated module; retained because the interpreter must not assume
    /// validation ran, and a silent misbehaviour here would corrupt a trace rather than fail.
    StackUnderflow,
    /// An operand had the wrong type. Unreachable for a validated module, as above.
    TypeMismatch,
    /// An instruction outside the admitted set reached the interpreter.
    ///
    /// This is the check that keeps [`crate::validate`]'s allowlist and the interpreter's
    /// coverage the same set: if the allowlist ever grows without the interpreter following,
    /// modules start trapping here instead of silently doing something else.
    Unsupported {
        /// The operator, rendered for a human.
        operator: String,
    },
}

impl core::fmt::Display for Trap {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Unreachable => write!(f, "unreachable executed"),
            Self::DivideByZero => write!(f, "integer divide by zero"),
            Self::IntegerOverflow => write!(f, "integer overflow"),
            Self::BadConversion => write!(f, "invalid conversion to integer"),
            Self::MemoryOutOfBounds => write!(f, "out of bounds memory access"),
            Self::TableOutOfBounds => write!(f, "out of bounds table access"),
            Self::UninitializedElement => write!(f, "uninitialized element"),
            Self::SignatureMismatch => write!(f, "indirect call type mismatch"),
            Self::CallStackExhausted => write!(f, "call stack exhausted"),
            Self::OutOfFuel => write!(f, "out of fuel"),
            Self::StackUnderflow => write!(f, "operand stack underflow"),
            Self::TypeMismatch => write!(f, "operand type mismatch"),
            Self::Unsupported { operator } => {
                write!(f, "unsupported instruction `{operator}`")
            }
        }
    }
}

impl std::error::Error for Trap {}

/// The operand stack an instruction acts on.
pub type Stack = Vec<Value>;

fn pop(stack: &mut Stack) -> Result<Value, Trap> {
    stack.pop().ok_or(Trap::StackUnderflow)
}

fn pop_i32(stack: &mut Stack) -> Result<i32, Trap> {
    match pop(stack)? {
        Value::I32(v) => Ok(v),
        _ => Err(Trap::TypeMismatch),
    }
}

fn pop_i64(stack: &mut Stack) -> Result<i64, Trap> {
    match pop(stack)? {
        Value::I64(v) => Ok(v),
        _ => Err(Trap::TypeMismatch),
    }
}

fn pop_f32(stack: &mut Stack) -> Result<f32, Trap> {
    match pop(stack)? {
        Value::F32(bits) => Ok(f32::from_bits(bits)),
        _ => Err(Trap::TypeMismatch),
    }
}

fn pop_f64(stack: &mut Stack) -> Result<f64, Trap> {
    match pop(stack)? {
        Value::F64(bits) => Ok(f64::from_bits(bits)),
        _ => Err(Trap::TypeMismatch),
    }
}

fn push_bool(stack: &mut Stack, value: bool) {
    stack.push(Value::I32(i32::from(value)));
}

fn push_f32(stack: &mut Stack, value: f32) {
    stack.push(Value::F32(value.to_bits()));
}

fn push_f64(stack: &mut Stack, value: f64) {
    stack.push(Value::F64(value.to_bits()));
}

/// `min` and `max` as WebAssembly defines them, which is not `if a < b`.
///
/// Two rules that an ordinary comparison gets wrong:
///
/// - **NaN in either operand gives NaN.** `a < b` is false when either is NaN, so a naive
///   implementation silently returns the second operand.
/// - **The sign of zero is directed.** `+0.0` and `-0.0` compare *equal*, so "return whichever
///   is smaller" has no opinion between them, yet `min` must yield `-0.0` and `max` `+0.0`.
///
/// Both fall out of [`f64::total_cmp`], whose total order already places `-0.0` below `+0.0`.
/// Using it rather than `==` and `<` makes the zero rule structural instead of a special case,
/// and avoids an equality comparison on floats that would be a mistake almost anywhere else.
macro_rules! wasm_min_max {
    ($min:ident, $max:ident, $ty:ty) => {
        fn $min(a: $ty, b: $ty) -> $ty {
            if a.is_nan() || b.is_nan() {
                return <$ty>::NAN;
            }
            if a.total_cmp(&b) == core::cmp::Ordering::Less {
                a
            } else {
                b
            }
        }

        fn $max(a: $ty, b: $ty) -> $ty {
            if a.is_nan() || b.is_nan() {
                return <$ty>::NAN;
            }
            if a.total_cmp(&b) == core::cmp::Ordering::Greater {
                a
            } else {
                b
            }
        }
    };
}

wasm_min_max!(wasm_min_f32, wasm_max_f32, f32);
wasm_min_max!(wasm_min_f64, wasm_max_f64, f64);

/// Truncate a float to an integer, trapping rather than saturating.
///
/// Rust's `as` cast saturates and maps NaN to zero, which is the `trunc_sat` behaviour. The
/// trapping forms therefore need the range check spelled out; borrowing `as` here would
/// silently turn a trap into a wrong answer.
///
/// # The bounds are exclusive, and that is not a detail
///
/// `$lo` and `$hi` are the first float values *outside* the target's range, and both
/// comparisons are inclusive so that those values themselves trap. Writing the check with
/// strict inequalities looks equivalent and is not: the boundary values are exactly
/// representable, so `-2147483904.0` — the first `f32` below `-2^31` — would slip through and
/// convert to a wrong answer rather than trapping.
///
/// The bounds are the neighbours of the range ends in the *source* float's precision, which is
/// why the same integer target has different bounds for `f32` and `f64`. At `2^31` an `f32`
/// steps by 256 and an `f64` by a fraction, so `i32.trunc_f32_s` bottoms out at
/// `-2147483904.0` while `i32.trunc_f64_s` bottoms out just past `-2147483649.0`.
macro_rules! trunc_trapping {
    ($value:expr, $lo:expr, $hi:expr, $target:ty) => {{
        let v = $value;
        if v.is_nan() {
            return Err(Trap::BadConversion);
        }
        let truncated = v.trunc();
        if truncated <= $lo || truncated >= $hi {
            return Err(Trap::BadConversion);
        }
        truncated as $target
    }};
}

/// Apply an operator if it is a numeric one.
///
/// Returns `Ok(true)` when the operator was handled, and `Ok(false)` when it belongs to
/// another category — control flow, variables, memory — for the caller to dispatch. Splitting
/// the dispatch this way keeps this module free of any machine state.
///
/// # Errors
///
/// Returns the [`Trap`] the instruction produces, if any.
#[expect(
    clippy::too_many_lines,
    reason = "one arm per WebAssembly numeric instruction; splitting by type would scatter \
              the mapping that most needs to be read as a single table"
)]
#[expect(
    clippy::float_cmp,
    reason = "the comparison instructions are defined as IEEE 754 comparisons; that is the \
              operation, not an accidental equality test on a computed float"
)]
pub fn apply(op: &Operator<'_>, stack: &mut Stack) -> Result<bool, Trap> {
    match op {
        // --- constants ------------------------------------------------------------------
        Operator::I32Const { value } => stack.push(Value::I32(*value)),
        Operator::I64Const { value } => stack.push(Value::I64(*value)),
        Operator::F32Const { value } => stack.push(Value::F32(value.bits())),
        Operator::F64Const { value } => stack.push(Value::F64(value.bits())),

        // --- i32 comparison -------------------------------------------------------------
        Operator::I32Eqz => {
            let a = pop_i32(stack)?;
            push_bool(stack, a == 0);
        }
        Operator::I32Eq => {
            let (b, a) = (pop_i32(stack)?, pop_i32(stack)?);
            push_bool(stack, a == b);
        }
        Operator::I32Ne => {
            let (b, a) = (pop_i32(stack)?, pop_i32(stack)?);
            push_bool(stack, a != b);
        }
        Operator::I32LtS => {
            let (b, a) = (pop_i32(stack)?, pop_i32(stack)?);
            push_bool(stack, a < b);
        }
        Operator::I32LtU => {
            let (b, a) = (pop_i32(stack)?, pop_i32(stack)?);
            push_bool(stack, (a as u32) < (b as u32));
        }
        Operator::I32GtS => {
            let (b, a) = (pop_i32(stack)?, pop_i32(stack)?);
            push_bool(stack, a > b);
        }
        Operator::I32GtU => {
            let (b, a) = (pop_i32(stack)?, pop_i32(stack)?);
            push_bool(stack, (a as u32) > (b as u32));
        }
        Operator::I32LeS => {
            let (b, a) = (pop_i32(stack)?, pop_i32(stack)?);
            push_bool(stack, a <= b);
        }
        Operator::I32LeU => {
            let (b, a) = (pop_i32(stack)?, pop_i32(stack)?);
            push_bool(stack, (a as u32) <= (b as u32));
        }
        Operator::I32GeS => {
            let (b, a) = (pop_i32(stack)?, pop_i32(stack)?);
            push_bool(stack, a >= b);
        }
        Operator::I32GeU => {
            let (b, a) = (pop_i32(stack)?, pop_i32(stack)?);
            push_bool(stack, (a as u32) >= (b as u32));
        }

        // --- i64 comparison -------------------------------------------------------------
        Operator::I64Eqz => {
            let a = pop_i64(stack)?;
            push_bool(stack, a == 0);
        }
        Operator::I64Eq => {
            let (b, a) = (pop_i64(stack)?, pop_i64(stack)?);
            push_bool(stack, a == b);
        }
        Operator::I64Ne => {
            let (b, a) = (pop_i64(stack)?, pop_i64(stack)?);
            push_bool(stack, a != b);
        }
        Operator::I64LtS => {
            let (b, a) = (pop_i64(stack)?, pop_i64(stack)?);
            push_bool(stack, a < b);
        }
        Operator::I64LtU => {
            let (b, a) = (pop_i64(stack)?, pop_i64(stack)?);
            push_bool(stack, (a as u64) < (b as u64));
        }
        Operator::I64GtS => {
            let (b, a) = (pop_i64(stack)?, pop_i64(stack)?);
            push_bool(stack, a > b);
        }
        Operator::I64GtU => {
            let (b, a) = (pop_i64(stack)?, pop_i64(stack)?);
            push_bool(stack, (a as u64) > (b as u64));
        }
        Operator::I64LeS => {
            let (b, a) = (pop_i64(stack)?, pop_i64(stack)?);
            push_bool(stack, a <= b);
        }
        Operator::I64LeU => {
            let (b, a) = (pop_i64(stack)?, pop_i64(stack)?);
            push_bool(stack, (a as u64) <= (b as u64));
        }
        Operator::I64GeS => {
            let (b, a) = (pop_i64(stack)?, pop_i64(stack)?);
            push_bool(stack, a >= b);
        }
        Operator::I64GeU => {
            let (b, a) = (pop_i64(stack)?, pop_i64(stack)?);
            push_bool(stack, (a as u64) >= (b as u64));
        }

        // --- float comparison -----------------------------------------------------------
        Operator::F32Eq => {
            let (b, a) = (pop_f32(stack)?, pop_f32(stack)?);
            push_bool(stack, a == b);
        }
        Operator::F32Ne => {
            let (b, a) = (pop_f32(stack)?, pop_f32(stack)?);
            push_bool(stack, a != b);
        }
        Operator::F32Lt => {
            let (b, a) = (pop_f32(stack)?, pop_f32(stack)?);
            push_bool(stack, a < b);
        }
        Operator::F32Gt => {
            let (b, a) = (pop_f32(stack)?, pop_f32(stack)?);
            push_bool(stack, a > b);
        }
        Operator::F32Le => {
            let (b, a) = (pop_f32(stack)?, pop_f32(stack)?);
            push_bool(stack, a <= b);
        }
        Operator::F32Ge => {
            let (b, a) = (pop_f32(stack)?, pop_f32(stack)?);
            push_bool(stack, a >= b);
        }
        Operator::F64Eq => {
            let (b, a) = (pop_f64(stack)?, pop_f64(stack)?);
            push_bool(stack, a == b);
        }
        Operator::F64Ne => {
            let (b, a) = (pop_f64(stack)?, pop_f64(stack)?);
            push_bool(stack, a != b);
        }
        Operator::F64Lt => {
            let (b, a) = (pop_f64(stack)?, pop_f64(stack)?);
            push_bool(stack, a < b);
        }
        Operator::F64Gt => {
            let (b, a) = (pop_f64(stack)?, pop_f64(stack)?);
            push_bool(stack, a > b);
        }
        Operator::F64Le => {
            let (b, a) = (pop_f64(stack)?, pop_f64(stack)?);
            push_bool(stack, a <= b);
        }
        Operator::F64Ge => {
            let (b, a) = (pop_f64(stack)?, pop_f64(stack)?);
            push_bool(stack, a >= b);
        }

        // --- i32 arithmetic -------------------------------------------------------------
        Operator::I32Clz => {
            let a = pop_i32(stack)?;
            stack.push(Value::I32(a.leading_zeros() as i32));
        }
        Operator::I32Ctz => {
            let a = pop_i32(stack)?;
            stack.push(Value::I32(a.trailing_zeros() as i32));
        }
        Operator::I32Popcnt => {
            let a = pop_i32(stack)?;
            stack.push(Value::I32(a.count_ones() as i32));
        }
        Operator::I32Add => {
            let (b, a) = (pop_i32(stack)?, pop_i32(stack)?);
            stack.push(Value::I32(a.wrapping_add(b)));
        }
        Operator::I32Sub => {
            let (b, a) = (pop_i32(stack)?, pop_i32(stack)?);
            stack.push(Value::I32(a.wrapping_sub(b)));
        }
        Operator::I32Mul => {
            let (b, a) = (pop_i32(stack)?, pop_i32(stack)?);
            stack.push(Value::I32(a.wrapping_mul(b)));
        }
        Operator::I32DivS => {
            let (b, a) = (pop_i32(stack)?, pop_i32(stack)?);
            if b == 0 {
                return Err(Trap::DivideByZero);
            }
            // The one signed division whose result is not representable.
            let result = a.checked_div(b).ok_or(Trap::IntegerOverflow)?;
            stack.push(Value::I32(result));
        }
        Operator::I32DivU => {
            let (b, a) = (pop_i32(stack)?, pop_i32(stack)?);
            if b == 0 {
                return Err(Trap::DivideByZero);
            }
            stack.push(Value::I32(((a as u32) / (b as u32)) as i32));
        }
        Operator::I32RemS => {
            let (b, a) = (pop_i32(stack)?, pop_i32(stack)?);
            if b == 0 {
                return Err(Trap::DivideByZero);
            }
            // Unlike div_s, INT_MIN % -1 does not trap: the answer is zero, and
            // wrapping_rem gives exactly that.
            stack.push(Value::I32(a.wrapping_rem(b)));
        }
        Operator::I32RemU => {
            let (b, a) = (pop_i32(stack)?, pop_i32(stack)?);
            if b == 0 {
                return Err(Trap::DivideByZero);
            }
            stack.push(Value::I32(((a as u32) % (b as u32)) as i32));
        }
        Operator::I32And => {
            let (b, a) = (pop_i32(stack)?, pop_i32(stack)?);
            stack.push(Value::I32(a & b));
        }
        Operator::I32Or => {
            let (b, a) = (pop_i32(stack)?, pop_i32(stack)?);
            stack.push(Value::I32(a | b));
        }
        Operator::I32Xor => {
            let (b, a) = (pop_i32(stack)?, pop_i32(stack)?);
            stack.push(Value::I32(a ^ b));
        }
        Operator::I32Shl => {
            let (b, a) = (pop_i32(stack)?, pop_i32(stack)?);
            stack.push(Value::I32(a.wrapping_shl(b as u32)));
        }
        Operator::I32ShrS => {
            let (b, a) = (pop_i32(stack)?, pop_i32(stack)?);
            stack.push(Value::I32(a.wrapping_shr(b as u32)));
        }
        Operator::I32ShrU => {
            let (b, a) = (pop_i32(stack)?, pop_i32(stack)?);
            stack.push(Value::I32(((a as u32).wrapping_shr(b as u32)) as i32));
        }
        Operator::I32Rotl => {
            let (b, a) = (pop_i32(stack)?, pop_i32(stack)?);
            stack.push(Value::I32(a.rotate_left((b as u32) % 32)));
        }
        Operator::I32Rotr => {
            let (b, a) = (pop_i32(stack)?, pop_i32(stack)?);
            stack.push(Value::I32(a.rotate_right((b as u32) % 32)));
        }

        // --- i64 arithmetic -------------------------------------------------------------
        Operator::I64Clz => {
            let a = pop_i64(stack)?;
            stack.push(Value::I64(i64::from(a.leading_zeros())));
        }
        Operator::I64Ctz => {
            let a = pop_i64(stack)?;
            stack.push(Value::I64(i64::from(a.trailing_zeros())));
        }
        Operator::I64Popcnt => {
            let a = pop_i64(stack)?;
            stack.push(Value::I64(i64::from(a.count_ones())));
        }
        Operator::I64Add => {
            let (b, a) = (pop_i64(stack)?, pop_i64(stack)?);
            stack.push(Value::I64(a.wrapping_add(b)));
        }
        Operator::I64Sub => {
            let (b, a) = (pop_i64(stack)?, pop_i64(stack)?);
            stack.push(Value::I64(a.wrapping_sub(b)));
        }
        Operator::I64Mul => {
            let (b, a) = (pop_i64(stack)?, pop_i64(stack)?);
            stack.push(Value::I64(a.wrapping_mul(b)));
        }
        Operator::I64DivS => {
            let (b, a) = (pop_i64(stack)?, pop_i64(stack)?);
            if b == 0 {
                return Err(Trap::DivideByZero);
            }
            let result = a.checked_div(b).ok_or(Trap::IntegerOverflow)?;
            stack.push(Value::I64(result));
        }
        Operator::I64DivU => {
            let (b, a) = (pop_i64(stack)?, pop_i64(stack)?);
            if b == 0 {
                return Err(Trap::DivideByZero);
            }
            stack.push(Value::I64(((a as u64) / (b as u64)) as i64));
        }
        Operator::I64RemS => {
            let (b, a) = (pop_i64(stack)?, pop_i64(stack)?);
            if b == 0 {
                return Err(Trap::DivideByZero);
            }
            stack.push(Value::I64(a.wrapping_rem(b)));
        }
        Operator::I64RemU => {
            let (b, a) = (pop_i64(stack)?, pop_i64(stack)?);
            if b == 0 {
                return Err(Trap::DivideByZero);
            }
            stack.push(Value::I64(((a as u64) % (b as u64)) as i64));
        }
        Operator::I64And => {
            let (b, a) = (pop_i64(stack)?, pop_i64(stack)?);
            stack.push(Value::I64(a & b));
        }
        Operator::I64Or => {
            let (b, a) = (pop_i64(stack)?, pop_i64(stack)?);
            stack.push(Value::I64(a | b));
        }
        Operator::I64Xor => {
            let (b, a) = (pop_i64(stack)?, pop_i64(stack)?);
            stack.push(Value::I64(a ^ b));
        }
        Operator::I64Shl => {
            let (b, a) = (pop_i64(stack)?, pop_i64(stack)?);
            stack.push(Value::I64(a.wrapping_shl(b as u32)));
        }
        Operator::I64ShrS => {
            let (b, a) = (pop_i64(stack)?, pop_i64(stack)?);
            stack.push(Value::I64(a.wrapping_shr(b as u32)));
        }
        Operator::I64ShrU => {
            let (b, a) = (pop_i64(stack)?, pop_i64(stack)?);
            stack.push(Value::I64(((a as u64).wrapping_shr(b as u32)) as i64));
        }
        Operator::I64Rotl => {
            let (b, a) = (pop_i64(stack)?, pop_i64(stack)?);
            stack.push(Value::I64(a.rotate_left((b as u64 % 64) as u32)));
        }
        Operator::I64Rotr => {
            let (b, a) = (pop_i64(stack)?, pop_i64(stack)?);
            stack.push(Value::I64(a.rotate_right((b as u64 % 64) as u32)));
        }

        // --- f32 arithmetic -------------------------------------------------------------
        Operator::F32Abs => {
            let a = pop_f32(stack)?;
            push_f32(stack, a.abs());
        }
        Operator::F32Neg => {
            let a = pop_f32(stack)?;
            push_f32(stack, -a);
        }
        Operator::F32Ceil => {
            let a = pop_f32(stack)?;
            push_f32(stack, a.ceil());
        }
        Operator::F32Floor => {
            let a = pop_f32(stack)?;
            push_f32(stack, a.floor());
        }
        Operator::F32Trunc => {
            let a = pop_f32(stack)?;
            push_f32(stack, a.trunc());
        }
        Operator::F32Nearest => {
            let a = pop_f32(stack)?;
            // Halves round to even, so nearest(2.5) is 2. `round` would give 3.
            push_f32(stack, a.round_ties_even());
        }
        Operator::F32Sqrt => {
            let a = pop_f32(stack)?;
            push_f32(stack, a.sqrt());
        }
        Operator::F32Add => {
            let (b, a) = (pop_f32(stack)?, pop_f32(stack)?);
            push_f32(stack, a + b);
        }
        Operator::F32Sub => {
            let (b, a) = (pop_f32(stack)?, pop_f32(stack)?);
            push_f32(stack, a - b);
        }
        Operator::F32Mul => {
            let (b, a) = (pop_f32(stack)?, pop_f32(stack)?);
            push_f32(stack, a * b);
        }
        Operator::F32Div => {
            let (b, a) = (pop_f32(stack)?, pop_f32(stack)?);
            push_f32(stack, a / b);
        }
        Operator::F32Min => {
            let (b, a) = (pop_f32(stack)?, pop_f32(stack)?);
            push_f32(stack, wasm_min_f32(a, b));
        }
        Operator::F32Max => {
            let (b, a) = (pop_f32(stack)?, pop_f32(stack)?);
            push_f32(stack, wasm_max_f32(a, b));
        }
        Operator::F32Copysign => {
            let (b, a) = (pop_f32(stack)?, pop_f32(stack)?);
            push_f32(stack, a.copysign(b));
        }

        // --- f64 arithmetic -------------------------------------------------------------
        Operator::F64Abs => {
            let a = pop_f64(stack)?;
            push_f64(stack, a.abs());
        }
        Operator::F64Neg => {
            let a = pop_f64(stack)?;
            push_f64(stack, -a);
        }
        Operator::F64Ceil => {
            let a = pop_f64(stack)?;
            push_f64(stack, a.ceil());
        }
        Operator::F64Floor => {
            let a = pop_f64(stack)?;
            push_f64(stack, a.floor());
        }
        Operator::F64Trunc => {
            let a = pop_f64(stack)?;
            push_f64(stack, a.trunc());
        }
        Operator::F64Nearest => {
            let a = pop_f64(stack)?;
            push_f64(stack, a.round_ties_even());
        }
        Operator::F64Sqrt => {
            let a = pop_f64(stack)?;
            push_f64(stack, a.sqrt());
        }
        Operator::F64Add => {
            let (b, a) = (pop_f64(stack)?, pop_f64(stack)?);
            push_f64(stack, a + b);
        }
        Operator::F64Sub => {
            let (b, a) = (pop_f64(stack)?, pop_f64(stack)?);
            push_f64(stack, a - b);
        }
        Operator::F64Mul => {
            let (b, a) = (pop_f64(stack)?, pop_f64(stack)?);
            push_f64(stack, a * b);
        }
        Operator::F64Div => {
            let (b, a) = (pop_f64(stack)?, pop_f64(stack)?);
            push_f64(stack, a / b);
        }
        Operator::F64Min => {
            let (b, a) = (pop_f64(stack)?, pop_f64(stack)?);
            push_f64(stack, wasm_min_f64(a, b));
        }
        Operator::F64Max => {
            let (b, a) = (pop_f64(stack)?, pop_f64(stack)?);
            push_f64(stack, wasm_max_f64(a, b));
        }
        Operator::F64Copysign => {
            let (b, a) = (pop_f64(stack)?, pop_f64(stack)?);
            push_f64(stack, a.copysign(b));
        }

        // --- width conversion -----------------------------------------------------------
        Operator::I32WrapI64 => {
            let a = pop_i64(stack)?;
            stack.push(Value::I32(a as i32));
        }
        Operator::I64ExtendI32S => {
            let a = pop_i32(stack)?;
            stack.push(Value::I64(i64::from(a)));
        }
        Operator::I64ExtendI32U => {
            let a = pop_i32(stack)?;
            stack.push(Value::I64(i64::from(a as u32)));
        }
        Operator::F32DemoteF64 => {
            let a = pop_f64(stack)?;
            push_f32(stack, a as f32);
        }
        Operator::F64PromoteF32 => {
            let a = pop_f32(stack)?;
            push_f64(stack, f64::from(a));
        }

        // --- sign extension -------------------------------------------------------------
        Operator::I32Extend8S => {
            let a = pop_i32(stack)?;
            stack.push(Value::I32(i32::from(a as i8)));
        }
        Operator::I32Extend16S => {
            let a = pop_i32(stack)?;
            stack.push(Value::I32(i32::from(a as i16)));
        }
        Operator::I64Extend8S => {
            let a = pop_i64(stack)?;
            stack.push(Value::I64(i64::from(a as i8)));
        }
        Operator::I64Extend16S => {
            let a = pop_i64(stack)?;
            stack.push(Value::I64(i64::from(a as i16)));
        }
        Operator::I64Extend32S => {
            let a = pop_i64(stack)?;
            stack.push(Value::I64(i64::from(a as i32)));
        }

        // --- float to integer, trapping -------------------------------------------------
        Operator::I32TruncF32S => {
            let a = pop_f32(stack)?;
            stack.push(Value::I32(trunc_trapping!(
                a,
                -2147483904.0f32,
                2147483648.0f32,
                i32
            )));
        }
        Operator::I32TruncF32U => {
            let a = pop_f32(stack)?;
            stack.push(Value::I32(
                trunc_trapping!(a, -1.0f32, 4294967296.0f32, u32) as i32,
            ));
        }
        Operator::I32TruncF64S => {
            let a = pop_f64(stack)?;
            stack.push(Value::I32(trunc_trapping!(
                a,
                -2147483649.0f64,
                2147483648.0f64,
                i32
            )));
        }
        Operator::I32TruncF64U => {
            let a = pop_f64(stack)?;
            stack.push(Value::I32(
                trunc_trapping!(a, -1.0f64, 4294967296.0f64, u32) as i32,
            ));
        }
        Operator::I64TruncF32S => {
            let a = pop_f32(stack)?;
            stack.push(Value::I64(trunc_trapping!(
                a,
                -9223373136366403584.0f32,
                9223372036854775808.0f32,
                i64
            )));
        }
        Operator::I64TruncF32U => {
            let a = pop_f32(stack)?;
            stack.push(Value::I64(
                trunc_trapping!(a, -1.0f32, 18446744073709551616.0f32, u64) as i64,
            ));
        }
        Operator::I64TruncF64S => {
            let a = pop_f64(stack)?;
            stack.push(Value::I64(trunc_trapping!(
                a,
                -9223372036854777856.0f64,
                9223372036854775808.0f64,
                i64
            )));
        }
        Operator::I64TruncF64U => {
            let a = pop_f64(stack)?;
            stack.push(Value::I64(
                trunc_trapping!(a, -1.0f64, 18446744073709551616.0f64, u64) as i64,
            ));
        }

        // --- float to integer, saturating -----------------------------------------------
        // Rust's `as` cast between float and integer already saturates and maps NaN to zero,
        // which is exactly what these instructions specify.
        Operator::I32TruncSatF32S => {
            let a = pop_f32(stack)?;
            stack.push(Value::I32(a as i32));
        }
        Operator::I32TruncSatF32U => {
            let a = pop_f32(stack)?;
            stack.push(Value::I32((a as u32) as i32));
        }
        Operator::I32TruncSatF64S => {
            let a = pop_f64(stack)?;
            stack.push(Value::I32(a as i32));
        }
        Operator::I32TruncSatF64U => {
            let a = pop_f64(stack)?;
            stack.push(Value::I32((a as u32) as i32));
        }
        Operator::I64TruncSatF32S => {
            let a = pop_f32(stack)?;
            stack.push(Value::I64(a as i64));
        }
        Operator::I64TruncSatF32U => {
            let a = pop_f32(stack)?;
            stack.push(Value::I64((a as u64) as i64));
        }
        Operator::I64TruncSatF64S => {
            let a = pop_f64(stack)?;
            stack.push(Value::I64(a as i64));
        }
        Operator::I64TruncSatF64U => {
            let a = pop_f64(stack)?;
            stack.push(Value::I64((a as u64) as i64));
        }

        // --- integer to float -----------------------------------------------------------
        Operator::F32ConvertI32S => {
            let a = pop_i32(stack)?;
            push_f32(stack, a as f32);
        }
        Operator::F32ConvertI32U => {
            let a = pop_i32(stack)?;
            push_f32(stack, (a as u32) as f32);
        }
        Operator::F32ConvertI64S => {
            let a = pop_i64(stack)?;
            push_f32(stack, a as f32);
        }
        Operator::F32ConvertI64U => {
            let a = pop_i64(stack)?;
            push_f32(stack, (a as u64) as f32);
        }
        Operator::F64ConvertI32S => {
            let a = pop_i32(stack)?;
            push_f64(stack, f64::from(a));
        }
        Operator::F64ConvertI32U => {
            let a = pop_i32(stack)?;
            push_f64(stack, f64::from(a as u32));
        }
        Operator::F64ConvertI64S => {
            let a = pop_i64(stack)?;
            push_f64(stack, a as f64);
        }
        Operator::F64ConvertI64U => {
            let a = pop_i64(stack)?;
            push_f64(stack, (a as u64) as f64);
        }

        // --- reinterpretation -----------------------------------------------------------
        // Pure bit casts: no value changes, only its type. A NaN payload survives intact,
        // which is why these are not canonicalized by the instrumentation pass.
        Operator::I32ReinterpretF32 => match pop(stack)? {
            Value::F32(bits) => stack.push(Value::I32(bits as i32)),
            _ => return Err(Trap::TypeMismatch),
        },
        Operator::I64ReinterpretF64 => match pop(stack)? {
            Value::F64(bits) => stack.push(Value::I64(bits as i64)),
            _ => return Err(Trap::TypeMismatch),
        },
        Operator::F32ReinterpretI32 => {
            let a = pop_i32(stack)?;
            stack.push(Value::F32(a as u32));
        }
        Operator::F64ReinterpretI64 => {
            let a = pop_i64(stack)?;
            stack.push(Value::F64(a as u64));
        }

        // Not a numeric instruction; the caller dispatches it.
        _ => return Ok(false),
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use super::*;

    /// Apply one operator to a prepared stack and return the result.
    fn eval(op: Operator<'_>, inputs: &[Value]) -> Result<Vec<Value>, Trap> {
        let mut stack = inputs.to_vec();
        let handled = apply(&op, &mut stack)?;
        assert!(handled, "{op:?} should be handled as numeric");
        Ok(stack)
    }

    /// Apply one operator expected to leave exactly one value.
    fn eval1(op: Operator<'_>, inputs: &[Value]) -> Result<Value, Trap> {
        let out = eval(op, inputs)?;
        assert_eq!(out.len(), 1, "expected a single result, got {out:?}");
        Ok(out[0])
    }

    fn i32v(v: i32) -> Value {
        Value::I32(v)
    }
    fn i64v(v: i64) -> Value {
        Value::I64(v)
    }
    fn f32v(v: f32) -> Value {
        Value::from_f32(v)
    }
    fn f64v(v: f64) -> Value {
        Value::from_f64(v)
    }

    #[test]
    fn arithmetic_wraps_rather_than_overflowing() {
        assert_eq!(
            eval1(Operator::I32Add, &[i32v(i32::MAX), i32v(1)]).unwrap(),
            i32v(i32::MIN)
        );
        assert_eq!(
            eval1(Operator::I64Mul, &[i64v(i64::MAX), i64v(2)]).unwrap(),
            i64v(-2)
        );
    }

    #[test]
    fn operands_pop_in_the_right_order() {
        // Subtraction is the cheapest way to catch a reversed pop, and a reversed pop would
        // otherwise pass every commutative test in this file.
        assert_eq!(
            eval1(Operator::I32Sub, &[i32v(10), i32v(3)]).unwrap(),
            i32v(7)
        );
        assert_eq!(
            eval1(Operator::I32DivS, &[i32v(20), i32v(4)]).unwrap(),
            i32v(5)
        );
        assert_eq!(
            eval1(Operator::F64Sub, &[f64v(10.0), f64v(3.0)]).unwrap(),
            f64v(7.0)
        );
    }

    #[test]
    fn signed_division_traps_on_the_one_unrepresentable_quotient() {
        // INT_MIN / -1 is +2^31, which does not fit. This is the only overflow trap in
        // WebAssembly integer arithmetic; everything else wraps.
        assert_eq!(
            eval1(Operator::I32DivS, &[i32v(i32::MIN), i32v(-1)]).unwrap_err(),
            Trap::IntegerOverflow
        );
        assert_eq!(
            eval1(Operator::I64DivS, &[i64v(i64::MIN), i64v(-1)]).unwrap_err(),
            Trap::IntegerOverflow
        );
    }

    #[test]
    fn signed_remainder_does_not_trap_on_those_same_operands() {
        // The asymmetry that a plausible implementation gets wrong: INT_MIN % -1 is defined,
        // and it is zero. Sharing a checked_div-style guard between div and rem would trap
        // here instead.
        assert_eq!(
            eval1(Operator::I32RemS, &[i32v(i32::MIN), i32v(-1)]).unwrap(),
            i32v(0)
        );
        assert_eq!(
            eval1(Operator::I64RemS, &[i64v(i64::MIN), i64v(-1)]).unwrap(),
            i64v(0)
        );
    }

    #[test]
    fn division_and_remainder_by_zero_trap() {
        for op in [
            Operator::I32DivS,
            Operator::I32DivU,
            Operator::I32RemS,
            Operator::I32RemU,
        ] {
            assert_eq!(
                eval1(op.clone(), &[i32v(1), i32v(0)]).unwrap_err(),
                Trap::DivideByZero,
                "{op:?}"
            );
        }
    }

    #[test]
    fn shift_counts_are_taken_modulo_the_width() {
        // A shift by the width is a shift by zero, not a wipe to zero. Implementing this with
        // a plain `<<` would be undefined behaviour in C and a panic in debug Rust.
        assert_eq!(
            eval1(Operator::I32Shl, &[i32v(1), i32v(32)]).unwrap(),
            i32v(1)
        );
        assert_eq!(
            eval1(Operator::I32Shl, &[i32v(1), i32v(33)]).unwrap(),
            i32v(2)
        );
        assert_eq!(
            eval1(Operator::I64Shl, &[i64v(1), i64v(64)]).unwrap(),
            i64v(1)
        );
        // Arithmetic vs logical right shift on a negative value.
        assert_eq!(
            eval1(Operator::I32ShrS, &[i32v(-8), i32v(1)]).unwrap(),
            i32v(-4)
        );
        assert_eq!(
            eval1(Operator::I32ShrU, &[i32v(-8), i32v(1)]).unwrap(),
            i32v(0x7fff_fffc)
        );
    }

    #[test]
    fn rotations_wrap_bits_around() {
        assert_eq!(
            eval1(Operator::I32Rotl, &[i32v(0x1234_5678), i32v(8)]).unwrap(),
            i32v(0x3456_7812)
        );
        assert_eq!(
            eval1(Operator::I32Rotr, &[i32v(0x1234_5678), i32v(8)]).unwrap(),
            i32v(0x7812_3456_u32 as i32)
        );
        // Rotating by the full width is the identity.
        assert_eq!(
            eval1(Operator::I64Rotl, &[i64v(-1234), i64v(64)]).unwrap(),
            i64v(-1234)
        );
    }

    #[test]
    fn unsigned_comparisons_are_not_signed_ones() {
        // -1 is the largest u32 and the smallest i32.
        assert_eq!(
            eval1(Operator::I32LtS, &[i32v(-1), i32v(1)]).unwrap(),
            i32v(1)
        );
        assert_eq!(
            eval1(Operator::I32LtU, &[i32v(-1), i32v(1)]).unwrap(),
            i32v(0)
        );
        // Comparisons yield i32 regardless of operand width.
        assert_eq!(
            eval1(Operator::I64GtU, &[i64v(-1), i64v(1)]).unwrap(),
            i32v(1)
        );
        assert_eq!(
            eval1(Operator::I64GtS, &[i64v(-1), i64v(1)]).unwrap(),
            i32v(0)
        );
    }

    #[test]
    fn min_and_max_propagate_nan() {
        // Not `if a < b`: a NaN operand makes the result NaN regardless of position.
        for (a, b) in [(f32::NAN, 1.0f32), (1.0f32, f32::NAN)] {
            let out = eval1(Operator::F32Min, &[f32v(a), f32v(b)]).unwrap();
            let Value::F32(bits) = out else {
                panic!("expected f32")
            };
            assert!(f32::from_bits(bits).is_nan(), "min({a}, {b})");

            let out = eval1(Operator::F32Max, &[f32v(a), f32v(b)]).unwrap();
            let Value::F32(bits) = out else {
                panic!("expected f32")
            };
            assert!(f32::from_bits(bits).is_nan(), "max({a}, {b})");
        }
    }

    #[test]
    fn min_and_max_direct_the_sign_of_zero() {
        // +0.0 and -0.0 compare equal, so "return whichever came first" passes an equality
        // test and is still wrong. min must yield -0.0 and max +0.0, in either argument order.
        for (a, b) in [(0.0f64, -0.0f64), (-0.0f64, 0.0f64)] {
            assert_eq!(
                eval1(Operator::F64Min, &[f64v(a), f64v(b)]).unwrap(),
                f64v(-0.0),
                "min({a}, {b})"
            );
            assert_eq!(
                eval1(Operator::F64Max, &[f64v(a), f64v(b)]).unwrap(),
                f64v(0.0),
                "max({a}, {b})"
            );
        }
    }

    #[test]
    fn nearest_rounds_halves_to_even() {
        // Rust's `round` rounds halves away from zero and would be wrong here.
        for (input, expected) in [
            (0.5f64, 0.0f64),
            (1.5, 2.0),
            (2.5, 2.0),
            (3.5, 4.0),
            (-0.5, -0.0),
            (-1.5, -2.0),
            (-2.5, -2.0),
        ] {
            assert_eq!(
                eval1(Operator::F64Nearest, &[f64v(input)]).unwrap(),
                f64v(expected),
                "nearest({input})"
            );
        }
    }

    #[test]
    fn truncation_traps_exactly_at_the_boundary() {
        // The bounds are the first float values outside the target's range, and they must
        // themselves trap. Writing the check with strict inequalities lets -2147483904.0 --
        // the first f32 below -2^31 -- convert to a wrong answer instead of trapping.
        assert_eq!(
            eval1(Operator::I32TruncF32S, &[f32v(-2_147_483_648.0)]).unwrap(),
            i32v(i32::MIN),
            "the range end itself converts"
        );
        assert_eq!(
            eval1(Operator::I32TruncF32S, &[f32v(-2_147_483_904.0)]).unwrap_err(),
            Trap::BadConversion,
            "the first f32 below the range must trap"
        );
        assert_eq!(
            eval1(Operator::I32TruncF32S, &[f32v(2_147_483_520.0)]).unwrap(),
            i32v(2_147_483_520),
            "the largest f32 inside the range converts"
        );
        assert_eq!(
            eval1(Operator::I32TruncF32S, &[f32v(2_147_483_648.0)]).unwrap_err(),
            Trap::BadConversion
        );

        // Unsigned: fractions above -1 truncate toward zero and are fine; -1 itself is not.
        assert_eq!(
            eval1(Operator::I32TruncF32U, &[f32v(-0.9)]).unwrap(),
            i32v(0)
        );
        assert_eq!(
            eval1(Operator::I32TruncF32U, &[f32v(-1.0)]).unwrap_err(),
            Trap::BadConversion
        );

        // f64 sources have finer spacing, so the same integer target has different bounds.
        assert_eq!(
            eval1(Operator::I32TruncF64S, &[f64v(-2_147_483_648.0)]).unwrap(),
            i32v(i32::MIN)
        );
        assert_eq!(
            eval1(Operator::I32TruncF64S, &[f64v(-2_147_483_649.0)]).unwrap_err(),
            Trap::BadConversion
        );
    }

    #[test]
    fn truncation_traps_on_nan_and_infinity() {
        for input in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(
                eval1(Operator::I64TruncF64S, &[f64v(input)]).unwrap_err(),
                Trap::BadConversion,
                "{input}"
            );
        }
    }

    #[test]
    fn saturating_truncation_clamps_instead_of_trapping() {
        assert_eq!(
            eval1(Operator::I32TruncSatF64S, &[f64v(1e300)]).unwrap(),
            i32v(i32::MAX)
        );
        assert_eq!(
            eval1(Operator::I32TruncSatF64S, &[f64v(-1e300)]).unwrap(),
            i32v(i32::MIN)
        );
        assert_eq!(
            eval1(Operator::I32TruncSatF64S, &[f64v(f64::NAN)]).unwrap(),
            i32v(0),
            "NaN saturates to zero, it does not trap"
        );
        assert_eq!(
            eval1(Operator::I32TruncSatF64U, &[f64v(-5.0)]).unwrap(),
            i32v(0)
        );
    }

    #[test]
    fn reinterpretation_preserves_nan_payloads() {
        // A bit cast changes the type and nothing else. This is why the instrumentation pass
        // does not canonicalize after reinterpret: the payload is whatever the program put
        // there, and that is deterministic.
        let payload = 0x7ff8_0000_dead_beef_u64;
        let as_int = eval1(Operator::I64ReinterpretF64, &[Value::F64(payload)]).unwrap();
        assert_eq!(as_int, Value::I64(payload as i64));

        let back = eval1(Operator::F64ReinterpretI64, &[as_int]).unwrap();
        assert_eq!(back, Value::F64(payload));
    }

    #[test]
    fn sign_extension_widens_correctly() {
        assert_eq!(
            eval1(Operator::I32Extend8S, &[i32v(0xff)]).unwrap(),
            i32v(-1)
        );
        assert_eq!(
            eval1(Operator::I32Extend16S, &[i32v(0x8000)]).unwrap(),
            i32v(-32768)
        );
        assert_eq!(
            eval1(Operator::I64Extend32S, &[i64v(0x8000_0000)]).unwrap(),
            i64v(-2_147_483_648)
        );
        // The two i32 -> i64 widenings differ on negative inputs.
        assert_eq!(
            eval1(Operator::I64ExtendI32S, &[i32v(-1)]).unwrap(),
            i64v(-1)
        );
        assert_eq!(
            eval1(Operator::I64ExtendI32U, &[i32v(-1)]).unwrap(),
            i64v(0xffff_ffff)
        );
    }

    #[test]
    fn width_conversion_round_trips_what_it_can() {
        assert_eq!(
            eval1(Operator::I32WrapI64, &[i64v(0x1_0000_0001)]).unwrap(),
            i32v(1)
        );
        assert_eq!(
            eval1(Operator::F64PromoteF32, &[f32v(0.5)]).unwrap(),
            f64v(0.5)
        );
        assert_eq!(
            eval1(Operator::F32DemoteF64, &[f64v(0.5)]).unwrap(),
            f32v(0.5)
        );
    }

    #[test]
    fn unsigned_integer_to_float_is_not_signed() {
        assert_eq!(
            eval1(Operator::F64ConvertI32U, &[i32v(-1)]).unwrap(),
            f64v(4_294_967_295.0)
        );
        assert_eq!(
            eval1(Operator::F64ConvertI32S, &[i32v(-1)]).unwrap(),
            f64v(-1.0)
        );
    }

    #[test]
    fn constants_push_their_value() {
        assert_eq!(
            eval1(Operator::I32Const { value: 7 }, &[]).unwrap(),
            i32v(7)
        );
        assert_eq!(
            eval1(Operator::I64Const { value: -7 }, &[]).unwrap(),
            i64v(-7)
        );
    }

    #[test]
    fn non_numeric_operators_are_handed_back() {
        // The dispatch contract: `Ok(false)` means "not mine", so the machine can layer
        // control flow, variables and memory on top without this module knowing about them.
        let mut stack = Vec::new();
        assert_eq!(
            apply(&Operator::LocalGet { local_index: 0 }, &mut stack),
            Ok(false)
        );
        assert_eq!(apply(&Operator::Nop, &mut stack), Ok(false));
        assert!(
            stack.is_empty(),
            "an unhandled operator must not touch the stack"
        );
    }

    #[test]
    fn a_short_stack_is_an_error_not_a_panic() {
        assert_eq!(
            eval1(Operator::I32Add, &[i32v(1)]).unwrap_err(),
            Trap::StackUnderflow
        );
        assert_eq!(
            eval1(Operator::I32Add, &[]).unwrap_err(),
            Trap::StackUnderflow
        );
    }

    #[test]
    fn a_mistyped_operand_is_an_error_not_a_panic() {
        // Unreachable for a validated module, but the interpreter must not assume validation
        // ran: silently reinterpreting an i64 as an i32 here would corrupt a trace instead of
        // failing.
        assert_eq!(
            eval1(Operator::I32Add, &[i64v(1), i32v(2)]).unwrap_err(),
            Trap::TypeMismatch
        );
        assert_eq!(
            eval1(Operator::F32Abs, &[i32v(1)]).unwrap_err(),
            Trap::TypeMismatch
        );
        assert_eq!(
            eval1(Operator::I64ReinterpretF64, &[i64v(1)]).unwrap_err(),
            Trap::TypeMismatch
        );
    }

    #[test]
    fn every_trap_renders_a_message() {
        let samples = [
            Trap::Unreachable,
            Trap::DivideByZero,
            Trap::IntegerOverflow,
            Trap::BadConversion,
            Trap::MemoryOutOfBounds,
            Trap::TableOutOfBounds,
            Trap::UninitializedElement,
            Trap::SignatureMismatch,
            Trap::CallStackExhausted,
            Trap::OutOfFuel,
            Trap::StackUnderflow,
            Trap::TypeMismatch,
            Trap::Unsupported {
                operator: "v128.const".to_owned(),
            },
        ];
        for sample in samples {
            assert!(
                !sample.to_string().is_empty(),
                "empty message for {sample:?}"
            );
        }
    }
}
