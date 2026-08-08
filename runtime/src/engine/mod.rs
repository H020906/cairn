//! The two execution paths, and what they share.
//!
//! Cairn runs a work unit twice in only one circumstance: a dispute. Ordinarily a unit is
//! executed once, on the **fast path**, at whatever speed the host's WebAssembly engine can
//! manage. The **slow path** is an interpreter that exists to answer one question —
//! *what exactly happened at instruction `i`?* — and it is only ever asked during arbitration.
//!
//! Both paths execute the same instrumented binary produced by [`crate::canon`], and both must
//! produce identical [`crate::state::StateCommitment`] roots. That is the invariant the whole
//! protocol rests on, and it is checked by differential fuzzing rather than assumed.
//!
//! - [`image`] — decodes an instrumented module into a form an interpreter can execute,
//!   resolving control-flow targets ahead of time.
//! - [`numeric`] — the numeric instructions, as pure stack transformations. Separated from
//!   the machine because this is where transcription risk concentrates, and it can be tested
//!   exhaustively without one.

pub mod image;
pub mod numeric;
