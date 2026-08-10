//! The Cairn coordinator.
//!
//! A library as well as a binary, so that what the coordinator *decides* can be tested without
//! opening a socket. See [`grid`] — every decision is there, and [`api`] does nothing but
//! translate HTTP into calls on it. [`dispute`] holds the two ways a disagreement can be
//! settled: by asking the parties, and by giving up and doing the work.
//!
//! Why this is Rust rather than the Java `ARCHITECTURE.md` describes: **the referee executes.**
//! Adjudication rebuilds a machine from a state witness and steps it once, which is the
//! execution kernel called from the coordinator. See
//! [ADR-0010](../../docs/adr/0010-the-referee-executes-so-the-coordinator-is-rust.md).

pub mod api;
pub mod dispute;
pub mod grid;
pub mod journal;
