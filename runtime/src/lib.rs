//! # Cairn execution kernel
//!
//! Deterministic WebAssembly execution with Merkle-committed execution traces.
//!
//! This crate is the part of Cairn that makes cheap verification possible. A volunteer
//! executes a work unit once, at full speed, and returns the answer together with a
//! commitment to *how it got there*. When two volunteers disagree, the coordinator
//! bisects their commitments to the single instruction where they diverged and re-executes
//! only that instruction. Checking one instruction instead of a billion is the entire
//! economic argument for the project — see
//! [ADR-0001](https://github.com/cairn-compute/cairn/blob/main/docs/adr/0001-verification-by-dispute-not-replication.md).
//!
//! ## Determinism is a correctness precondition
//!
//! Two honest workers on different hardware, operating systems and browser engines must
//! produce **byte-identical** traces. If they cannot, arbitration convicts an innocent
//! volunteer — silently, rarely, and disproportionately on unusual hardware.
//!
//! Nothing in this crate may depend on wall-clock time, system entropy, thread scheduling,
//! memory addresses, or hash-map iteration order. This is enforced by lint where possible
//! and by differential fuzzing where it is not.
//!
//! ## Module layout
//!
//! - [`merkle`] — incremental Merkle commitment over linear memory pages. The structure
//!   that lets a snapshot cost `O(writes)` rather than `O(memory)`, and that lets the
//!   coordinator verify one page against a root without holding the whole image.
//! - [`fuel`] — the instruction counter that gives a trace its coordinate system.
//!   "Instruction *i*" must mean the same thing on every machine.
//!
//! - [`validate`] — decides which modules Cairn is willing to run at all. Its allowlist is
//!   deliberately the same set the interpreter implements, so no module can be accepted that
//!   we would later be unable to arbitrate.
//!
//! - [`canon`] — the instrumentation pass that rewrites a submitted module into
//!   Cairn-canonical form, so that both execution paths run the identical binary and have
//!   nothing left to disagree about.
//! - [`state`] — the canonical machine state and its commitment. The vocabulary the dispute
//!   protocol speaks: a snapshot is a state root, bisection compares two of them, and
//!   arbitration checks that one instruction moves one to the other.
//!
//! Still to land:
//!
//! - `engine` — the two execution paths. `fast` hands the instrumented module to the host
//!   engine and snapshots at `2^k` boundaries; `slow` is a fully instrumented interpreter
//!   able to execute a single instruction in isolation from a committed state.

pub mod canon;
pub mod fuel;
pub mod merkle;
pub mod state;
pub mod validate;
