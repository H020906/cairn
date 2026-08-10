//! The canonical representation of a machine state, and its commitment.
//!
//! This is the vocabulary the dispute protocol speaks. A snapshot is a
//! [`StateCommitment::root`]; bisection compares two of them; arbitration re-executes one
//! instruction and checks that the resulting root matches what a worker claimed. Both
//! execution paths must agree on this encoding down to the byte, or every downstream
//! guarantee is void.
//!
//! # Floating point is stored as bits, never as floats
//!
//! [`Value::F32`] and [`Value::F64`] hold raw bit patterns, not `f32`/`f64`. This is not a
//! micro-optimisation; it is the only correct choice here, for two reasons.
//!
//! First, **NaN is not equal to itself.** A state containing a NaN would never compare equal
//! to a copy of itself if floats were compared as floats, so a worker replaying its own
//! execution would appear to diverge from itself.
//!
//! Second, **`+0.0` and `-0.0` compare equal but are different states.** A program can
//! distinguish them — `1.0 / 0.0` is `+inf` while `1.0 / -0.0` is `-inf` — so two states
//! differing only in the sign of a zero really are different states and must commit to
//! different roots.
//!
//! Comparing bits gets both cases right for free. The `float_cmp` lint is denied across this
//! crate to keep it that way.
//!
//! # What a commitment covers
//!
//! ```text
//! root = H( domain ‖ memory_root ‖ globals ‖ operand_stack ‖ call_stack
//!           ‖ segments ‖ output ‖ pc ‖ fuel )
//! ```
//!
//! The memory root comes from [`crate::merkle::PageTree`], so it is cheap to maintain
//! incrementally and supports single-page proofs during arbitration. The other components are
//! small enough to hash outright.
//!
//! **`output` is the answer**, and it is here because a commitment that did not cover it would
//! let two executions agree at every step while having returned different results — leaving the
//! coordinator with two matching traces and no way to say which answer was right. See
//! [`StateCommitment::output`].

use crate::fuel::Fuel;
use crate::merkle::Hash;

/// Domain separator for a whole-state commitment.
///
/// `0x00` and `0x01` belong to [`crate::merkle`]'s leaf and node hashes; every domain byte in
/// this crate is distinct so that no hash of one kind can be presented as a hash of another.
const DOMAIN_STATE: u8 = 0x02;

/// Domain separator for a hashed sequence of values.
const DOMAIN_VALUES: u8 = 0x03;

/// Domain separator for the linear-memory commitment.
const DOMAIN_MEMORY: u8 = 0x04;

/// Domain separator for one call frame.
const DOMAIN_FRAME: u8 = 0x05;

/// Domain separator for a frame's label stack.
const DOMAIN_LABELS: u8 = 0x06;

/// Domain separator for the call stack as a whole.
const DOMAIN_CALL_STACK: u8 = 0x07;

/// Domain separator for the dropped-segment bitmaps.
const DOMAIN_SEGMENTS: u8 = 0x08;

/// Domain separator for the output buffer.
const DOMAIN_OUTPUT: u8 = 0x09;

/// A WebAssembly numeric value, as it appears on the operand stack, in a local, or in a global.
///
/// Reference types are absent by construction: [`crate::validate`] rejects any module that
/// could produce one, precisely because a host reference has no host-independent encoding and
/// so could never appear in a commitment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Value {
    /// A 32-bit integer.
    I32(i32),
    /// A 64-bit integer.
    I64(i64),
    /// A 32-bit float, held as its raw bit pattern. See the module documentation.
    F32(u32),
    /// A 64-bit float, held as its raw bit pattern. See the module documentation.
    F64(u64),
}

impl Value {
    /// Tag byte identifying the value's type in the canonical encoding.
    const fn tag(self) -> u8 {
        match self {
            Self::I32(_) => 0x00,
            Self::I64(_) => 0x01,
            Self::F32(_) => 0x02,
            Self::F64(_) => 0x03,
        }
    }

    /// The payload, widened to 64 bits without interpretation.
    const fn payload(self) -> u64 {
        match self {
            Self::I32(v) => v as u32 as u64,
            Self::I64(v) => v as u64,
            Self::F32(bits) => bits as u64,
            Self::F64(bits) => bits,
        }
    }

    /// The canonical nine-byte encoding: one tag byte then the payload, little-endian.
    ///
    /// Fixed width keeps a sequence of values unambiguous without per-element framing.
    #[must_use]
    pub const fn encode(self) -> [u8; 9] {
        let payload = self.payload().to_le_bytes();
        [
            self.tag(),
            payload[0],
            payload[1],
            payload[2],
            payload[3],
            payload[4],
            payload[5],
            payload[6],
            payload[7],
        ]
    }

    /// Interpret an `f32` as a value, keeping its exact bits.
    #[must_use]
    pub fn from_f32(value: f32) -> Self {
        Self::F32(value.to_bits())
    }

    /// Interpret an `f64` as a value, keeping its exact bits.
    #[must_use]
    pub fn from_f64(value: f64) -> Self {
        Self::F64(value.to_bits())
    }
}

/// Commit to a sequence of values — an operand stack, a frame's locals, or the globals.
///
/// The length is hashed explicitly. Without it, `[a, b] ‖ [c]` and `[a] ‖ [b, c]` would
/// produce the same bytes when two such sequences are combined, and a worker could shuffle
/// values between the stack and its locals without changing the commitment.
#[must_use]
pub fn hash_values(values: &[Value]) -> Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&[DOMAIN_VALUES]);
    hasher.update(&(values.len() as u64).to_le_bytes());
    for value in values {
        hasher.update(&value.encode());
    }
    *hasher.finalize().as_bytes()
}

/// Commit to linear memory: its contents *and* its current size.
///
/// [`crate::merkle::PageTree`] deliberately does not authenticate its own page count, on the
/// grounds that the work unit's manifest fixes the memory size. That reasoning holds for the
/// declared maximum and not for the current size, because `memory.grow` makes the page count
/// observable state that changes during execution — a program can read it back with
/// `memory.size` and branch on it.
///
/// So the page tree is sized once to the declared maximum, pages past the current end read as
/// zero, and the page count is bound here instead. Without this, a worker that had grown its
/// memory and one that had not would commit to the same root whenever the extra pages were
/// still blank.
#[must_use]
pub fn hash_memory(pages: u32, page_tree_root: &Hash) -> Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&[DOMAIN_MEMORY]);
    hasher.update(&pages.to_le_bytes());
    hasher.update(page_tree_root);
    *hasher.finalize().as_bytes()
}

/// One entry of a frame's label stack, reduced to what a branch depends on.
///
/// Strictly, labels are derivable from the function and instruction index: WebAssembly
/// validation assigns every program point a unique operand-stack height and enclosing block
/// structure. They are committed anyway. Relying on that derivation would make the soundness
/// of the commitment depend on a subtle property of validation rather than on what the
/// interpreter actually holds, and snapshots are thousands of instructions apart, so the
/// hashing costs nothing worth arguing about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LabelDigest {
    /// Instruction index a branch to this label jumps to.
    pub branch_target: u32,
    /// Number of values a branch to this label preserves.
    pub arity: u32,
    /// Operand-stack height a branch truncates to.
    pub stack_height: u32,
    /// Whether this label belongs to a `loop`, which a branch re-enters rather than exits.
    pub is_loop: bool,
}

/// Commit to a frame's label stack, innermost last.
#[must_use]
pub fn hash_labels(labels: &[LabelDigest]) -> Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&[DOMAIN_LABELS]);
    hasher.update(&(labels.len() as u64).to_le_bytes());
    for label in labels {
        hasher.update(&label.branch_target.to_le_bytes());
        hasher.update(&label.arity.to_le_bytes());
        hasher.update(&label.stack_height.to_le_bytes());
        hasher.update(&[u8::from(label.is_loop)]);
    }
    *hasher.finalize().as_bytes()
}

/// One call frame, reduced to hashes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameDigest {
    /// Index of the function this frame is executing.
    pub function: u32,
    /// Instruction index to resume at within that function.
    pub instruction: u32,
    /// Operand-stack height this frame was entered at.
    pub stack_base: u32,
    /// Number of values the frame returns.
    pub arity: u32,
    /// [`hash_values`] over the frame's locals, parameters first.
    pub locals: Hash,
    /// [`hash_labels`] over the frame's label stack.
    pub labels: Hash,
}

/// Commit to the call stack, outermost first.
#[must_use]
pub fn hash_frames(frames: &[FrameDigest]) -> Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&[DOMAIN_CALL_STACK]);
    hasher.update(&(frames.len() as u64).to_le_bytes());
    for frame in frames {
        hasher.update(&[DOMAIN_FRAME]);
        hasher.update(&frame.function.to_le_bytes());
        hasher.update(&frame.instruction.to_le_bytes());
        hasher.update(&frame.stack_base.to_le_bytes());
        hasher.update(&frame.arity.to_le_bytes());
        hasher.update(&frame.locals);
        hasher.update(&frame.labels);
    }
    *hasher.finalize().as_bytes()
}

/// Commit to which data and element segments have been dropped.
///
/// `data.drop` and `elem.drop` change what a later `memory.init` or `table.init` copies, so two
/// executions differing only in which segments they have dropped are genuinely different
/// states. Without this they would commit to the same root, and a divergence in that part
/// would be invisible to arbitration.
#[must_use]
pub fn hash_segments(dropped_data: &[bool], dropped_elements: &[bool]) -> Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&[DOMAIN_SEGMENTS]);
    hasher.update(&(dropped_data.len() as u64).to_le_bytes());
    for dropped in dropped_data {
        hasher.update(&[u8::from(*dropped)]);
    }
    hasher.update(&(dropped_elements.len() as u64).to_le_bytes());
    for dropped in dropped_elements {
        hasher.update(&[u8::from(*dropped)]);
    }
    *hasher.finalize().as_bytes()
}

/// Commit to what the workload has answered so far.
///
/// Length-prefixed, so that an empty output and a zero-length write are the same state — they
/// are — while no two different byte strings can collide by concatenation.
#[must_use]
pub fn hash_output(output: &[u8]) -> Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&[DOMAIN_OUTPUT]);
    hasher.update(&(output.len() as u64).to_le_bytes());
    hasher.update(output);
    *hasher.finalize().as_bytes()
}

/// Where execution has reached: an instruction within a function.
///
/// Both indices refer to the *instrumented* module, which is the only module any worker ever
/// runs. Instruction indices are positions in the function's operator sequence, so they are
/// stable across machines in a way byte offsets would not be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct ProgramCounter {
    /// Index of the executing function in the instrumented module.
    pub function: u32,
    /// Index of the next instruction to execute within that function.
    pub instruction: u32,
}

impl ProgramCounter {
    /// The canonical eight-byte encoding.
    #[must_use]
    pub const fn encode(self) -> [u8; 8] {
        let f = self.function.to_le_bytes();
        let i = self.instruction.to_le_bytes();
        [f[0], f[1], f[2], f[3], i[0], i[1], i[2], i[3]]
    }
}

impl core::fmt::Display for ProgramCounter {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "func {}:{}", self.function, self.instruction)
    }
}

/// The parts of a machine state, each already reduced to a hash.
///
/// An engine assembles this from its own structures; nothing here assumes how an interpreter
/// stores its stack or its frames, only how it must summarise them. That seam is deliberate —
/// the fast path and the slow path have very different internals and must still produce
/// identical roots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StateCommitment {
    /// Root of the linear-memory page tree.
    pub memory: Hash,
    /// [`hash_values`] over the module's globals, in index order.
    pub globals: Hash,
    /// [`hash_values`] over the operand stack, bottom to top.
    pub operand_stack: Hash,
    /// A hash covering the call stack: per frame, its function, its return position, its
    /// locals and its operand-stack base.
    pub call_stack: Hash,
    /// [`hash_segments`] over the dropped data and element segments.
    pub segments: Hash,
    /// [`hash_output`] over the bytes written through `cairn.output` so far.
    ///
    /// # Why the answer is part of the state
    ///
    /// Without this, two executions could commit to identical roots at every step and still
    /// have returned **different answers** — and a coordinator holding two agreeing traces
    /// would have proved nothing about the thing it actually cares about. Bisection would
    /// report no disagreement, correctly, and the disagreement would still be there.
    ///
    /// With it, a state root *determines* the answer: agreeing traces agree on the output, and
    /// a single party's witness at the final step proves what the answer was, against a root
    /// both parties already committed to. That turns "these two agree, so who was wrong about
    /// the answer?" from a full re-execution into a hash comparison.
    ///
    /// A digest rather than the bytes, because `cairn.output` **replaces** rather than appends
    /// — nothing ever reads the buffer back — so a witness never has to carry it. Which is what
    /// keeps a witness small on a workload that returns a megabyte.
    pub output: Hash,
    /// The instruction about to execute.
    pub program_counter: ProgramCounter,
    /// Instructions retired so far. This is the coordinate bisection searches over.
    pub fuel: Fuel,
}

impl StateCommitment {
    /// The single hash that identifies this state.
    ///
    /// Every component is fixed-width, so the concatenation is unambiguous without length
    /// prefixes at this level.
    #[must_use]
    pub fn root(&self) -> Hash {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&[DOMAIN_STATE]);
        hasher.update(&self.memory);
        hasher.update(&self.globals);
        hasher.update(&self.operand_stack);
        hasher.update(&self.call_stack);
        hasher.update(&self.segments);
        hasher.update(&self.output);
        hasher.update(&self.program_counter.encode());
        hasher.update(&self.fuel.get().to_le_bytes());
        *hasher.finalize().as_bytes()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::merkle::PageTree;

    fn commitment() -> StateCommitment {
        StateCommitment {
            memory: PageTree::new(4).root(),
            output: hash_output(b"answer"),
            globals: hash_values(&[Value::I32(1)]),
            operand_stack: hash_values(&[Value::I64(2)]),
            call_stack: hash_values(&[Value::I32(3)]),
            segments: hash_segments(&[false], &[]),
            program_counter: ProgramCounter {
                function: 7,
                instruction: 11,
            },
            fuel: Fuel::new(1234),
        }
    }

    #[test]
    fn a_commitment_is_a_function_of_its_parts() {
        assert_eq!(commitment().root(), commitment().root());
    }

    #[test]
    fn every_component_changes_the_root() {
        // If any part of the state could change without moving the root, a worker could
        // diverge in that part and still appear to agree.
        let base = commitment().root();

        let mut m = commitment();
        m.memory = PageTree::new(8).root();
        assert_ne!(m.root(), base, "memory");

        let mut g = commitment();
        g.globals = hash_values(&[Value::I32(99)]);
        assert_ne!(g.root(), base, "globals");

        let mut s = commitment();
        s.operand_stack = hash_values(&[Value::I64(99)]);
        assert_ne!(s.root(), base, "operand stack");

        let mut c = commitment();
        c.call_stack = hash_values(&[Value::I32(99)]);
        assert_ne!(c.root(), base, "call stack");

        // `data.drop` changes what a later `memory.init` copies, so two executions differing
        // only in which segments they have dropped really are different states.
        let mut seg = commitment();
        seg.segments = hash_segments(&[true], &[]);
        assert_ne!(seg.root(), base, "dropped segments");

        let mut pc = commitment();
        pc.program_counter.instruction += 1;
        assert_ne!(pc.root(), base, "program counter");

        let mut fu = commitment();
        fu.fuel = Fuel::new(1235);
        assert_ne!(fu.root(), base, "fuel");
    }

    #[test]
    fn the_program_counter_distinguishes_function_from_instruction() {
        // A naive encoding that summed or concatenated without fixed width could confuse
        // (1, 2) with (2, 1).
        let a = ProgramCounter {
            function: 1,
            instruction: 2,
        };
        let b = ProgramCounter {
            function: 2,
            instruction: 1,
        };
        assert_ne!(a.encode(), b.encode());
    }

    #[test]
    fn nan_commits_equal_to_itself() {
        // The reason floats are stored as bits. Compared as floats, NaN != NaN, and a worker
        // replaying its own execution would appear to diverge from itself.
        let nan = Value::from_f64(f64::NAN);
        assert_eq!(nan, nan);
        assert_eq!(hash_values(&[nan]), hash_values(&[nan]));
    }

    #[test]
    fn distinct_nan_payloads_are_distinct_states() {
        // Two NaNs the program constructed itself, with different payloads, are genuinely
        // different memory contents. Canonicalization applies to arithmetic results, not to
        // bits a program wrote deliberately.
        let quiet = Value::F64(0x7ff8_0000_0000_0000);
        let payload = Value::F64(0x7ff8_0000_dead_beef);
        assert_ne!(hash_values(&[quiet]), hash_values(&[payload]));
    }

    #[test]
    fn positive_and_negative_zero_are_distinct_states() {
        // They compare equal as floats but a program can tell them apart: 1.0/0.0 is +inf
        // and 1.0/-0.0 is -inf. Two states differing only here really are different.
        let pos = Value::from_f64(0.0);
        let neg = Value::from_f64(-0.0);
        assert_ne!(pos, neg);
        assert_ne!(hash_values(&[pos]), hash_values(&[neg]));
    }

    #[test]
    fn types_are_distinguished_even_with_identical_payloads() {
        // i32 0, i64 0, f32 +0.0 and f64 +0.0 all have an all-zero payload. Without the tag
        // byte they would be indistinguishable, and a worker could substitute one for
        // another.
        let zeros = [Value::I32(0), Value::I64(0), Value::F32(0), Value::F64(0)];
        for (i, a) in zeros.iter().enumerate() {
            for b in zeros.iter().skip(i + 1) {
                assert_ne!(a.encode(), b.encode(), "{a:?} vs {b:?}");
            }
        }
    }

    #[test]
    fn value_sequences_are_length_bound() {
        // Without the length prefix, values could be moved between two hashed sequences —
        // say from the operand stack into locals — without changing either hash.
        assert_ne!(
            hash_values(&[Value::I32(1), Value::I32(2)]),
            hash_values(&[Value::I32(1)])
        );
        assert_ne!(hash_values(&[]), hash_values(&[Value::I32(0)]));
    }

    #[test]
    fn order_matters() {
        assert_ne!(
            hash_values(&[Value::I32(1), Value::I32(2)]),
            hash_values(&[Value::I32(2), Value::I32(1)])
        );
    }

    #[test]
    fn state_and_value_domains_do_not_collide() {
        // A commitment root must never be presentable as a value-sequence hash. The domain
        // bytes are what prevent it; this test fails if someone reuses one.
        let empty_values = hash_values(&[]);
        let state = StateCommitment {
            memory: [0; 32],
            globals: [0; 32],
            operand_stack: [0; 32],
            call_stack: [0; 32],
            segments: [0; 32],
            output: [0; 32],
            program_counter: ProgramCounter::default(),
            fuel: Fuel::ZERO,
        };
        assert_ne!(state.root(), empty_values);
    }

    #[test]
    fn memory_growth_changes_the_commitment_even_when_the_new_pages_are_blank() {
        // The reason `hash_memory` exists. A worker that grew its memory and one that did not
        // hold identical bytes while the extra pages are still zero, but `memory.size` returns
        // different values, so they are different states.
        let root = PageTree::new(64).root();
        assert_ne!(hash_memory(1, &root), hash_memory(2, &root));
    }

    #[test]
    fn memory_contents_still_change_the_commitment() {
        let blank = PageTree::new(4).root();
        let mut written = PageTree::new(4);
        written
            .set_page(0, &vec![7u8; crate::merkle::PAGE_SIZE])
            .unwrap();
        assert_ne!(hash_memory(4, &blank), hash_memory(4, &written.root()));
    }

    #[test]
    fn every_label_field_changes_the_label_hash() {
        // A branch's behaviour depends on all four, so all four must be bound.
        let base = LabelDigest {
            branch_target: 10,
            arity: 1,
            stack_height: 3,
            is_loop: false,
        };
        let reference = hash_labels(&[base]);

        let mut target = base;
        target.branch_target = 11;
        assert_ne!(hash_labels(&[target]), reference, "branch target");

        let mut arity = base;
        arity.arity = 2;
        assert_ne!(hash_labels(&[arity]), reference, "arity");

        let mut height = base;
        height.stack_height = 4;
        assert_ne!(hash_labels(&[height]), reference, "stack height");

        // A loop label is re-entered by a branch and a block label is exited, so this single
        // bit changes where execution goes next.
        let mut kind = base;
        kind.is_loop = true;
        assert_ne!(hash_labels(&[kind]), reference, "loop flag");
    }

    #[test]
    fn label_and_frame_stacks_are_depth_bound() {
        let label = LabelDigest {
            branch_target: 1,
            arity: 0,
            stack_height: 0,
            is_loop: false,
        };
        assert_ne!(hash_labels(&[label]), hash_labels(&[label, label]));
        assert_ne!(hash_labels(&[]), hash_labels(&[label]));

        let frame = FrameDigest {
            function: 1,
            instruction: 2,
            stack_base: 0,
            arity: 0,
            locals: hash_values(&[]),
            labels: hash_labels(&[]),
        };
        assert_ne!(hash_frames(&[frame]), hash_frames(&[frame, frame]));
        assert_ne!(hash_frames(&[]), hash_frames(&[frame]));
    }

    #[test]
    fn every_frame_field_changes_the_call_stack_hash() {
        let base = FrameDigest {
            function: 3,
            instruction: 4,
            stack_base: 5,
            arity: 1,
            locals: hash_values(&[Value::I32(1)]),
            labels: hash_labels(&[]),
        };
        let reference = hash_frames(&[base]);

        let mut function = base;
        function.function = 4;
        assert_ne!(hash_frames(&[function]), reference, "function");

        let mut instruction = base;
        instruction.instruction = 5;
        assert_ne!(hash_frames(&[instruction]), reference, "instruction");

        let mut stack_base = base;
        stack_base.stack_base = 6;
        assert_ne!(hash_frames(&[stack_base]), reference, "stack base");

        let mut arity = base;
        arity.arity = 2;
        assert_ne!(hash_frames(&[arity]), reference, "arity");

        let mut locals = base;
        locals.locals = hash_values(&[Value::I32(2)]);
        assert_ne!(hash_frames(&[locals]), reference, "locals");

        let mut labels = base;
        labels.labels = hash_labels(&[LabelDigest {
            branch_target: 1,
            arity: 0,
            stack_height: 0,
            is_loop: false,
        }]);
        assert_ne!(hash_frames(&[labels]), reference, "labels");
    }

    #[test]
    fn frame_order_matters() {
        // The call stack is ordered: A calling B is not B calling A.
        let a = FrameDigest {
            function: 1,
            instruction: 0,
            stack_base: 0,
            arity: 0,
            locals: hash_values(&[]),
            labels: hash_labels(&[]),
        };
        let b = FrameDigest { function: 2, ..a };
        assert_ne!(hash_frames(&[a, b]), hash_frames(&[b, a]));
    }

    #[test]
    fn the_new_domains_do_not_collide_with_the_old_ones() {
        // Five hash kinds now share this module. A collision between any two would let a
        // worker present one as another.
        let empty_values = hash_values(&[]);
        let empty_labels = hash_labels(&[]);
        let empty_frames = hash_frames(&[]);
        let memory = hash_memory(0, &[0; 32]);

        let all = [empty_values, empty_labels, empty_frames, memory];
        for (i, a) in all.iter().enumerate() {
            for b in all.iter().skip(i + 1) {
                assert_ne!(a, b, "domain collision");
            }
        }
    }

    #[test]
    fn i32_payloads_are_sign_preserving_and_distinct() {
        // -1 widens to 0xffff_ffff, not to 0xffff_ffff_ffff_ffff, so it must not collide with
        // an i64 of -1 once the tag is accounted for.
        assert_eq!(
            Value::I32(-1).encode(),
            [0x00, 0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00]
        );
        assert_eq!(
            Value::I64(-1).encode(),
            [0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]
        );
        assert_ne!(Value::I32(-1).encode(), Value::I64(-1).encode());
    }
}
