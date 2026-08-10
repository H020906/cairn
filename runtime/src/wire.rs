//! Putting a state witness on a wire, and taking one off a hostile one.
//!
//! A dispute is settled by executing one instruction from the state both parties agreed on.
//! The coordinator does not have that state and must not compute it — reaching step *n* costs
//! `O(n)`, which is the cost bisection exists to avoid. So **a party hands it over**, and this
//! module is the format it travels in.
//!
//! # Why a witness may be decoded from an untrusted party
//!
//! Because it is checked afterwards, by exactly one comparison.
//! [`Witness::commitment`] rebuilds a [`crate::state::StateCommitment`] from the witness alone,
//! and [`crate::dispute::adjudicate`] refuses any witness whose root differs from the one
//! bisection already established both parties claimed. A fabricated witness therefore cannot
//! decide a dispute; it can only fail to be accepted.
//!
//! That check is what this decoder is *not* responsible for. Its job is narrower and entirely
//! about self-defence:
//!
//! - **Never panic.** A malformed length prefix must be an error, not a slice out of range.
//!   The coordinator is a long-lived process settling other people's disputes.
//! - **Never allocate on a stranger's say-so.** Every count is checked against the bytes that
//!   remain before anything is reserved.
//! - **Never hang.** There are no loops here whose trip count is not bounded by the input.
//!
//! # Why allocation needs no limit constant
//!
//! Each element has a minimum encoded size, so a count of `n` elements requires at least
//! `n × minimum` bytes of input to be legitimate. Checking that before reserving makes the
//! largest possible allocation a small multiple of the input length — and the input length is
//! bounded by whoever read it off the socket. A `max_pages`-style constant would be a second
//! place to get the number wrong.
//!
//! # Format
//!
//! Little-endian, no padding, no self-description beyond the header. It is a wire format
//! between two programs built from this repository, not an interchange format.
//!
//! ```text
//! magic    "CWTN"
//! version  u8 = 2
//! output   32 bytes (the digest of the answer so far)
//! globals        u32 count, then count × value
//! operand stack  u32 count, then count × value
//! frames         u32 count, then count × frame
//! memory_pages       u32
//! memory_max_pages   u32
//! memory_root        32 bytes
//! pages          u32 count, then count × page
//! dropped_data       u32 count, then count × u8 (0 or 1)
//! dropped_elements   u32 count, then count × u8
//! fuel     u64
//! steps    u64
//!
//! value  u8 tag (0 = i32, 1 = i64, 2 = f32, 3 = f64), then u64 payload
//! frame  u32 function, u32 instruction, u32 stack_base, u32 arity,
//!        u32 count × value (locals), u32 count × label
//! label  u32 branch_target, u32 arity, u32 stack_height, u32 end, u8 is_loop
//! page   u32 index, PAGE_SIZE bytes, u32 count × 32-byte sibling
//! ```
//!
//! # This encoding is not the commitment encoding
//!
//! [`crate::state`] hashes values with its own tags, and nothing here has to agree with them —
//! the round trip is checked against the witness, not against a hash. Keeping the two separate
//! means a future wire change cannot silently move a state root, which would convict honest
//! volunteers on old software rather than fail to parse.

use crate::engine::machine::{FrameWitness, LabelWitness, PageWitness, Witness};
use crate::fuel::Fuel;
use crate::merkle::{Hash, PAGE_SIZE};
use crate::state::Value;

/// Identifies the format, so a truncated body or an unrelated POST fails immediately rather
/// than as a confusing count.
const MAGIC: [u8; 4] = *b"CWTN";

/// The only version this build speaks.
///
/// Bumped to 2 when the answer became part of the committed state: a witness now carries an
/// output digest, and a version-1 witness would reconstruct a *different* root rather than
/// fail to parse. Refusing it outright is the difference between "your worker is out of date"
/// and an unexplained conviction.
const VERSION: u8 = 2;

/// Smallest number of bytes one encoded value can occupy.
const VALUE_MIN: usize = 9;
/// Smallest number of bytes one encoded label can occupy.
const LABEL_MIN: usize = 17;
/// Smallest number of bytes one encoded frame can occupy: four fixed words and two empty counts.
const FRAME_MIN: usize = 24;
/// Smallest number of bytes one encoded page can occupy: index, the page itself, an empty proof.
const PAGE_MIN: usize = 4 + PAGE_SIZE + 4;

/// Why a witness could not be encoded or decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireError {
    /// The bytes do not begin with this format's magic.
    NotAWitness,
    /// A version this build does not speak.
    UnsupportedVersion {
        /// The version byte found.
        found: u8,
    },
    /// The input ended in the middle of a field.
    Truncated {
        /// What was being read when it ran out.
        reading: &'static str,
    },
    /// A count claims more elements than the remaining input could hold.
    ///
    /// The defence against a length prefix of four billion. Checked *before* reserving.
    ImplausibleCount {
        /// What was being read.
        reading: &'static str,
        /// The count claimed.
        claimed: u64,
        /// Bytes actually left.
        remaining: usize,
    },
    /// A value tag outside `0..=3`. Reference types cannot appear in a Cairn witness at all —
    /// [`crate::validate`] rejects any module that could produce one.
    UnknownValueTag {
        /// The tag byte found.
        tag: u8,
    },
    /// A page carried something other than exactly [`PAGE_SIZE`] bytes.
    WrongPageLength {
        /// How many bytes it carried.
        found: usize,
    },
    /// Bytes remain after a complete witness. Refused rather than ignored: trailing data means
    /// the sender and this decoder disagree about the format, and guessing which is right is
    /// how a parser becomes an attack surface.
    TrailingBytes {
        /// How many.
        extra: usize,
    },
    /// A count exceeded `u32`, so it cannot be encoded. Only reachable from a witness this
    /// process built, which makes it a bug here rather than a stranger's doing.
    TooLargeToEncode {
        /// What was being written.
        writing: &'static str,
    },
}

impl core::fmt::Display for WireError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotAWitness => write!(f, "not a Cairn witness"),
            Self::UnsupportedVersion { found } => {
                write!(f, "witness format version {found} is not supported")
            }
            Self::Truncated { reading } => write!(f, "witness ended while reading {reading}"),
            Self::ImplausibleCount {
                reading,
                claimed,
                remaining,
            } => write!(
                f,
                "{reading} claims {claimed} entries with only {remaining} bytes left"
            ),
            Self::UnknownValueTag { tag } => write!(f, "unknown value tag {tag:#04x}"),
            Self::WrongPageLength { found } => {
                write!(f, "a page carried {found} bytes, not {PAGE_SIZE}")
            }
            Self::TrailingBytes { extra } => write!(f, "{extra} bytes after the witness"),
            Self::TooLargeToEncode { writing } => write!(f, "too many {writing} to encode"),
        }
    }
}

impl std::error::Error for WireError {}

// --- encoding ---------------------------------------------------------------------------------

/// Serialise a witness.
///
/// # Errors
///
/// [`WireError::WrongPageLength`] if a page is not exactly [`PAGE_SIZE`] bytes, and
/// [`WireError::TooLargeToEncode`] if some collection exceeds `u32`. Both mean the witness was
/// built wrongly by this process rather than that anything arrived badly.
pub fn encode(witness: &Witness) -> Result<Vec<u8>, WireError> {
    let mut out = Vec::new();
    out.extend_from_slice(&MAGIC);
    out.push(VERSION);
    out.extend_from_slice(&witness.output);

    put_values(&mut out, &witness.globals, "globals")?;
    put_values(&mut out, &witness.operand_stack, "operand stack entries")?;

    put_count(&mut out, witness.frames.len(), "frames")?;
    for frame in &witness.frames {
        out.extend_from_slice(&frame.function.to_le_bytes());
        out.extend_from_slice(&frame.instruction.to_le_bytes());
        out.extend_from_slice(&frame.stack_base.to_le_bytes());
        out.extend_from_slice(&frame.arity.to_le_bytes());
        put_values(&mut out, &frame.locals, "locals")?;
        put_count(&mut out, frame.labels.len(), "labels")?;
        for label in &frame.labels {
            out.extend_from_slice(&label.branch_target.to_le_bytes());
            out.extend_from_slice(&label.arity.to_le_bytes());
            out.extend_from_slice(&label.stack_height.to_le_bytes());
            out.extend_from_slice(&label.end.to_le_bytes());
            out.push(u8::from(label.is_loop));
        }
    }

    out.extend_from_slice(&witness.memory_pages.to_le_bytes());
    out.extend_from_slice(&witness.memory_max_pages.to_le_bytes());
    out.extend_from_slice(&witness.memory_root);

    put_count(&mut out, witness.pages.len(), "pages")?;
    for page in &witness.pages {
        if page.bytes.len() != PAGE_SIZE {
            return Err(WireError::WrongPageLength {
                found: page.bytes.len(),
            });
        }
        out.extend_from_slice(&page.index.to_le_bytes());
        out.extend_from_slice(&page.bytes);
        put_count(&mut out, page.proof.len(), "proof siblings")?;
        for sibling in &page.proof {
            out.extend_from_slice(sibling);
        }
    }

    put_flags(&mut out, &witness.dropped_data, "dropped data segments")?;
    put_flags(
        &mut out,
        &witness.dropped_elements,
        "dropped element segments",
    )?;

    out.extend_from_slice(&witness.fuel.get().to_le_bytes());
    out.extend_from_slice(&witness.steps.to_le_bytes());
    Ok(out)
}

fn put_count(out: &mut Vec<u8>, count: usize, writing: &'static str) -> Result<(), WireError> {
    let count = u32::try_from(count).map_err(|_| WireError::TooLargeToEncode { writing })?;
    out.extend_from_slice(&count.to_le_bytes());
    Ok(())
}

fn put_values(out: &mut Vec<u8>, values: &[Value], writing: &'static str) -> Result<(), WireError> {
    put_count(out, values.len(), writing)?;
    for value in values {
        let (tag, payload) = match *value {
            Value::I32(v) => (0u8, u64::from(v as u32)),
            Value::I64(v) => (1u8, v as u64),
            Value::F32(bits) => (2u8, u64::from(bits)),
            Value::F64(bits) => (3u8, bits),
        };
        out.push(tag);
        out.extend_from_slice(&payload.to_le_bytes());
    }
    Ok(())
}

fn put_flags(out: &mut Vec<u8>, flags: &[bool], writing: &'static str) -> Result<(), WireError> {
    put_count(out, flags.len(), writing)?;
    out.extend(flags.iter().map(|f| u8::from(*f)));
    Ok(())
}

// --- decoding ---------------------------------------------------------------------------------

/// Parse a witness produced by [`encode`], from bytes that may be anything at all.
///
/// The result is **not trusted** and is not meant to be: whether it describes the state the
/// parties agreed on is settled by [`crate::dispute::adjudicate`], which compares its
/// commitment against the agreed root. What this guarantees is only that a hostile body
/// produces an error rather than a panic, an allocation storm, or a hang.
///
/// # Errors
///
/// See [`WireError`].
pub fn decode(bytes: &[u8]) -> Result<Witness, WireError> {
    let mut r = Reader::new(bytes);

    if r.take(4, "magic")? != MAGIC {
        return Err(WireError::NotAWitness);
    }
    let version = r.u8("version")?;
    if version != VERSION {
        return Err(WireError::UnsupportedVersion { found: version });
    }

    let output = r.hash("output digest")?;
    let globals = r.values("globals")?;
    let operand_stack = r.values("operand stack")?;

    let frame_count = r.count("frames", FRAME_MIN)?;
    let mut frames = Vec::with_capacity(frame_count);
    for _ in 0..frame_count {
        let function = r.u32("frame function")?;
        let instruction = r.u32("frame instruction")?;
        let stack_base = r.u32("frame stack base")?;
        let arity = r.u32("frame arity")?;
        let locals = r.values("locals")?;

        let label_count = r.count("labels", LABEL_MIN)?;
        let mut labels = Vec::with_capacity(label_count);
        for _ in 0..label_count {
            labels.push(LabelWitness {
                branch_target: r.u32("label target")?,
                arity: r.u32("label arity")?,
                stack_height: r.u32("label stack height")?,
                end: r.u32("label end")?,
                is_loop: r.u8("label kind")? != 0,
            });
        }

        frames.push(FrameWitness {
            function,
            instruction,
            stack_base,
            arity,
            locals,
            labels,
        });
    }

    let memory_pages = r.u32("memory pages")?;
    let memory_max_pages = r.u32("memory ceiling")?;
    let memory_root = r.hash("memory root")?;

    let page_count = r.count("pages", PAGE_MIN)?;
    let mut pages = Vec::with_capacity(page_count);
    for _ in 0..page_count {
        let index = r.u32("page index")?;
        let bytes = r.take(PAGE_SIZE, "page bytes")?.to_vec();
        let proof_len = r.count("proof siblings", 32)?;
        let mut proof = Vec::with_capacity(proof_len);
        for _ in 0..proof_len {
            proof.push(r.hash("proof sibling")?);
        }
        pages.push(PageWitness {
            index,
            bytes,
            proof,
        });
    }

    let dropped_data = r.flags("dropped data segments")?;
    let dropped_elements = r.flags("dropped element segments")?;
    let fuel = Fuel::new(r.u64("fuel")?);
    let steps = r.u64("steps")?;

    if r.remaining() != 0 {
        return Err(WireError::TrailingBytes {
            extra: r.remaining(),
        });
    }

    Ok(Witness {
        output,
        globals,
        operand_stack,
        frames,
        memory_pages,
        memory_max_pages,
        memory_root,
        pages,
        dropped_data,
        dropped_elements,
        fuel,
        steps,
    })
}

/// A cursor that runs out of input rather than off the end of it.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.at)
    }

    fn take(&mut self, n: usize, reading: &'static str) -> Result<&'a [u8], WireError> {
        let end = self
            .at
            .checked_add(n)
            .ok_or(WireError::Truncated { reading })?;
        let slice = self
            .bytes
            .get(self.at..end)
            .ok_or(WireError::Truncated { reading })?;
        self.at = end;
        Ok(slice)
    }

    fn u8(&mut self, reading: &'static str) -> Result<u8, WireError> {
        self.take(1, reading)?
            .first()
            .copied()
            .ok_or(WireError::Truncated { reading })
    }

    fn u32(&mut self, reading: &'static str) -> Result<u32, WireError> {
        let bytes: [u8; 4] = self
            .take(4, reading)?
            .try_into()
            .map_err(|_| WireError::Truncated { reading })?;
        Ok(u32::from_le_bytes(bytes))
    }

    fn u64(&mut self, reading: &'static str) -> Result<u64, WireError> {
        let bytes: [u8; 8] = self
            .take(8, reading)?
            .try_into()
            .map_err(|_| WireError::Truncated { reading })?;
        Ok(u64::from_le_bytes(bytes))
    }

    fn hash(&mut self, reading: &'static str) -> Result<Hash, WireError> {
        self.take(32, reading)?
            .try_into()
            .map_err(|_| WireError::Truncated { reading })
    }

    /// A length prefix, refused unless the remaining input could plausibly hold it.
    ///
    /// This is the whole defence. `Vec::with_capacity` on an unchecked `u32` is a four-gigabyte
    /// allocation for five bytes of attacker input, which is a denial of service that costs
    /// nothing to mount and would take the coordinator down mid-dispute.
    fn count(&mut self, reading: &'static str, element_min: usize) -> Result<usize, WireError> {
        let claimed = u64::from(self.u32(reading)?);
        let needed = claimed.saturating_mul(element_min as u64);
        if needed > self.remaining() as u64 {
            return Err(WireError::ImplausibleCount {
                reading,
                claimed,
                remaining: self.remaining(),
            });
        }
        usize::try_from(claimed).map_err(|_| WireError::ImplausibleCount {
            reading,
            claimed,
            remaining: self.remaining(),
        })
    }

    fn values(&mut self, reading: &'static str) -> Result<Vec<Value>, WireError> {
        let count = self.count(reading, VALUE_MIN)?;
        let mut values = Vec::with_capacity(count);
        for _ in 0..count {
            let tag = self.u8("value tag")?;
            let payload = self.u64("value payload")?;
            values.push(match tag {
                0 => Value::I32(payload as u32 as i32),
                1 => Value::I64(payload as i64),
                2 => Value::F32(payload as u32),
                3 => Value::F64(payload),
                tag => return Err(WireError::UnknownValueTag { tag }),
            });
        }
        Ok(values)
    }

    fn flags(&mut self, reading: &'static str) -> Result<Vec<bool>, WireError> {
        let count = self.count(reading, 1)?;
        Ok(self.take(count, reading)?.iter().map(|b| *b != 0).collect())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use super::*;
    use crate::canon::{self, Config};
    use crate::engine::image;
    use crate::engine::machine::{Limits, Machine, Progress};

    /// A workload that writes to memory, so its witnesses carry pages and proofs rather than
    /// only small state. A round trip over the easy shape would prove very little.
    const STORING: &str = r#"
        (module
          (import "cairn" "input"  (func $input  (param i32 i32) (result i32)))
          (import "cairn" "output" (func $output (param i32 i32)))
          (memory (export "memory") 4 8)
          (global $g (mut i64) (i64.const 7))
          (func (export "cairn_run") (local $i i32) (local $len i32)
            (local.set $len (call $input (i32.const 0) (i32.const 0)))
            (drop (call $input (i32.const 0) (local.get $len)))
            (block $done
              (loop $again
                (br_if $done (i32.ge_u (local.get $i) (i32.const 900)))
                (i32.store (local.get $i) (local.get $i))
                (global.set $g (i64.add (global.get $g) (i64.const 1)))
                (local.set $i (i32.add (local.get $i) (i32.const 4)))
                (br $again)))
            (call $output (i32.const 0) (i32.const 8))))
    "#;

    fn canonical(text: &str) -> Vec<u8> {
        let source = wat::parse_str(text).expect("module should assemble");
        crate::validate::validate_submitted(&source, crate::validate::Limits::default())
            .expect("module should be admissible");
        canon::instrument(&source, Config::dispute_path()).expect("instrumentation should succeed")
    }

    #[test]
    fn every_witness_of_a_real_execution_survives_the_round_trip() {
        // Not one witness — every witness, at every step, including the ones carrying pages and
        // the ones carrying none. A format that round-trips the first instruction and loses a
        // proof sibling at the two-hundredth would pass a smaller test.
        let module = canonical(STORING);
        let image = image::decode(&module).unwrap();
        let mut machine = Machine::new(&image, b"round trip".to_vec(), Limits::default()).unwrap();

        let mut carried_pages = 0;
        let mut carried_none = 0;
        for step in 0..4_000u64 {
            let witness = machine.witness_for_next_step();
            if witness.pages.is_empty() {
                carried_none += 1;
            } else {
                carried_pages += 1;
            }

            let bytes = encode(&witness).unwrap();
            let back = decode(&bytes).unwrap_or_else(|e| panic!("step {step}: {e}"));
            assert_eq!(back, witness, "witness at step {step} did not round-trip");

            // The property that actually matters downstream: a decoded witness must commit to
            // the same root, or adjudication would refuse a witness that arrived intact.
            assert_eq!(
                back.commitment().root(),
                witness.commitment().root(),
                "step {step}: the decoded witness commits differently"
            );

            if matches!(machine.step().unwrap(), Progress::Finished) {
                break;
            }
        }
        assert!(carried_pages > 0, "no witness carried a page");
        assert!(carried_none > 0, "every witness carried a page");
    }

    /// A witness with something in every field, built by hand so the test does not depend on
    /// which shapes a particular workload happens to produce.
    fn populated() -> Witness {
        Witness {
            output: crate::state::hash_output(b"an answer"),
            globals: vec![
                Value::I32(-1),
                Value::I64(i64::MIN),
                Value::F32(0x7fc0_0000),
            ],
            operand_stack: vec![Value::F64(u64::MAX), Value::I32(0)],
            frames: vec![
                FrameWitness {
                    function: 3,
                    instruction: 11,
                    locals: vec![Value::I64(-9)],
                    stack_base: 1,
                    arity: 2,
                    labels: vec![LabelWitness {
                        branch_target: 4,
                        arity: 1,
                        stack_height: 3,
                        end: 90,
                        is_loop: true,
                    }],
                },
                FrameWitness {
                    function: 0,
                    instruction: 0,
                    locals: Vec::new(),
                    stack_base: 0,
                    arity: 0,
                    labels: Vec::new(),
                },
            ],
            memory_pages: 2,
            memory_max_pages: 8,
            memory_root: [9; 32],
            pages: vec![PageWitness {
                index: 1,
                bytes: vec![0xab; PAGE_SIZE],
                proof: vec![[1; 32], [2; 32]],
            }],
            dropped_data: vec![true, false, true],
            dropped_elements: vec![false],
            fuel: Fuel::new(123_456),
            steps: 78_910,
        }
    }

    #[test]
    fn a_hand_built_witness_round_trips_field_for_field() {
        let witness = populated();
        assert_eq!(decode(&encode(&witness).unwrap()).unwrap(), witness);
    }

    #[test]
    fn truncating_anywhere_is_an_error_and_never_a_panic() {
        // Every prefix of a valid encoding. A decoder that reads one field past the end is the
        // ordinary way this kind of code fails, and it fails on exactly one of these.
        let bytes = encode(&populated()).unwrap();
        for cut in 0..bytes.len() {
            let prefix = &bytes[..cut];
            assert!(
                decode(prefix).is_err(),
                "a {cut}-byte prefix decoded as a whole witness"
            );
        }
        assert!(decode(&bytes).is_ok(), "the whole thing must still decode");
    }

    #[test]
    fn flipping_any_single_byte_is_an_error_or_a_different_witness_but_never_a_panic() {
        // The point is the "never a panic". Most corruptions produce a witness that is
        // well-formed and wrong — which is fine, because adjudication checks the commitment —
        // and the rest must be clean errors.
        let bytes = encode(&populated()).unwrap();
        // The page body is 64 KiB of identical filler; walking all of it would add a minute to
        // the suite and test the same branch 65,000 times. Header, counts and trailer are where
        // the parsing decisions are.
        let interesting: Vec<usize> = (0..64)
            .chain(bytes.len().saturating_sub(400)..bytes.len())
            .collect();
        for at in interesting {
            for mask in [0x01u8, 0x80, 0xff] {
                let mut corrupted = bytes.clone();
                corrupted[at] ^= mask;
                // Must return, either way.
                let _ = decode(&corrupted);
            }
        }
    }

    #[test]
    fn a_count_larger_than_the_input_is_refused_before_anything_is_allocated() {
        // The attack this decoder exists to survive: five bytes claiming four billion entries.
        // Left unchecked, `Vec::with_capacity` turns that into tens of gigabytes.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&MAGIC);
        bytes.push(VERSION);
        bytes.extend_from_slice(&[0; 32]); // output digest
        bytes.extend_from_slice(&u32::MAX.to_le_bytes()); // globals

        match decode(&bytes) {
            Err(WireError::ImplausibleCount { claimed, .. }) => {
                assert_eq!(claimed, u64::from(u32::MAX));
            }
            other => panic!("expected the count to be refused, got {other:?}"),
        }
    }

    #[test]
    fn every_count_in_the_format_is_bounded_the_same_way() {
        // Not just the first one. A single unchecked prefix anywhere is the whole hole, so this
        // walks the encoding and overwrites each 4-byte count in turn with u32::MAX.
        let witness = populated();
        let good = encode(&witness).unwrap();

        // Offsets of the length prefixes, derived rather than hard-coded, by encoding witnesses
        // that differ only in one collection's length and finding where the bytes first differ.
        let mut probes = Vec::new();
        let mut with_extra_global = witness.clone();
        with_extra_global.globals.push(Value::I32(1));
        probes.push(with_extra_global);
        let mut with_extra_stack = witness.clone();
        with_extra_stack.operand_stack.push(Value::I32(1));
        probes.push(with_extra_stack);
        let mut with_extra_frame = witness.clone();
        with_extra_frame.frames.push(FrameWitness {
            function: 0,
            instruction: 0,
            locals: Vec::new(),
            stack_base: 0,
            arity: 0,
            labels: Vec::new(),
        });
        probes.push(with_extra_frame);
        let mut with_extra_flag = witness.clone();
        with_extra_flag.dropped_data.push(false);
        probes.push(with_extra_flag);

        for probe in probes {
            let other = encode(&probe).unwrap();
            let at = good
                .iter()
                .zip(other.iter())
                .position(|(a, b)| a != b)
                .expect("the encodings must differ somewhere");
            let mut hostile = good.clone();
            hostile[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
            assert!(
                matches!(decode(&hostile), Err(WireError::ImplausibleCount { .. })),
                "an unbounded count at byte {at} was not refused"
            );
        }
    }

    #[test]
    fn random_bytes_never_decode_and_never_panic() {
        // Not a fuzzer — `runtime/tests/admission.rs` is that. This is the cheap always-on
        // version: a deterministic pseudo-random sweep that runs on every `cargo test`.
        let mut state = 0x243f_6a88_85a3_08d3u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };

        for case in 0..2_000u32 {
            let len = (next() % 512) as usize;
            let mut bytes: Vec<u8> = (0..len).map(|_| (next() & 0xff) as u8).collect();
            // Half the cases wear the right header, so they get past the cheap rejection and
            // exercise the counts underneath.
            if case % 2 == 0 && bytes.len() >= 5 {
                bytes[..4].copy_from_slice(&MAGIC);
                bytes[4] = VERSION;
            }
            let _ = decode(&bytes);
        }
    }

    #[test]
    fn trailing_bytes_are_refused_rather_than_ignored() {
        let mut bytes = encode(&populated()).unwrap();
        bytes.push(0);
        assert_eq!(
            decode(&bytes).unwrap_err(),
            WireError::TrailingBytes { extra: 1 }
        );
    }

    #[test]
    fn the_header_is_checked_before_anything_else() {
        assert_eq!(
            decode(b"").unwrap_err(),
            WireError::Truncated { reading: "magic" }
        );
        assert_eq!(decode(b"nope").unwrap_err(), WireError::NotAWitness);

        let mut wrong_version = encode(&populated()).unwrap();
        wrong_version[4] = 99;
        assert_eq!(
            decode(&wrong_version).unwrap_err(),
            WireError::UnsupportedVersion { found: 99 }
        );

        // Version 1 is refused rather than read. It had no output digest, so a permissive
        // decoder would produce a witness that reconstructs a *different* root — which reaches
        // adjudication as "this witness does not match the agreed state" and looks like the
        // party lied. A version check is what makes that an upgrade notice instead.
        let mut version_one = encode(&populated()).unwrap();
        version_one[4] = 1;
        assert_eq!(
            decode(&version_one).unwrap_err(),
            WireError::UnsupportedVersion { found: 1 }
        );
    }

    #[test]
    fn an_unknown_value_tag_is_refused() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&MAGIC);
        bytes.push(VERSION);
        bytes.extend_from_slice(&[0; 32]); // output digest
        bytes.extend_from_slice(&1u32.to_le_bytes()); // one global
        bytes.push(0x7f); // no such type
        bytes.extend_from_slice(&0u64.to_le_bytes());
        assert_eq!(
            decode(&bytes).unwrap_err(),
            WireError::UnknownValueTag { tag: 0x7f }
        );
    }

    #[test]
    fn a_page_of_the_wrong_length_cannot_be_encoded() {
        let mut witness = populated();
        witness.pages[0].bytes.truncate(10);
        assert_eq!(
            encode(&witness).unwrap_err(),
            WireError::WrongPageLength { found: 10 }
        );
    }

    #[test]
    fn every_error_renders_a_message() {
        let samples = [
            WireError::NotAWitness,
            WireError::UnsupportedVersion { found: 2 },
            WireError::Truncated { reading: "magic" },
            WireError::ImplausibleCount {
                reading: "pages",
                claimed: 9,
                remaining: 1,
            },
            WireError::UnknownValueTag { tag: 0x40 },
            WireError::WrongPageLength { found: 3 },
            WireError::TrailingBytes { extra: 7 },
            WireError::TooLargeToEncode { writing: "frames" },
        ];
        for sample in samples {
            assert!(
                !sample.to_string().is_empty(),
                "empty message for {sample:?}"
            );
        }
    }
}
