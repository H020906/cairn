//! What the coordinator writes down, so that killing it does not lose the work people did for it.
//!
//! # Why this is an append-only log and not SQLite
//!
//! `ARCHITECTURE.md` says SQLite, and the roadmap said SQLite. Writing it revealed that the
//! coordinator has no use for a database, and the reason is worth keeping.
//!
//! **There are no queries.** Every read in [`crate::grid`] is already a linear scan of an
//! in-memory `Vec` — `lease` walks the units, `dispute_for` walks the disputes — because the
//! whole state fits in memory and is expected to. Persistence here is not "where the data lives";
//! it is "how the in-memory state is rebuilt after a restart". That is a log, not a table.
//!
//! **The project's dependency rule is that a dependency must do something the standard library
//! cannot.** Durably appending a record and replaying the file is not that. `rusqlite` would
//! bring a bundled C amalgamation into the one component that decides who is convicted of
//! cheating, to buy indexes nothing indexes and transactions over a single writer.
//!
//! What SQLite *would* buy is somebody else's crash-safety testing, and that is a real argument
//! rather than a straw one. The counter is the shape of this file's failure mode: writes are
//! sequential appends, so a crash can only tear the **last** record, and truncating at the first
//! record that fails its checksum is the whole of recovery. The full statement of the trade is in
//! [ADR-0014](../../docs/adr/0014-the-coordinator-keeps-a-log-not-a-database.md).
//!
//! # Leases, and what a restart does to an argument
//!
//! **Leases are written down, but not durably, and they come back expired.** The first draft of
//! this file skipped them on the reasoning that a lease is a promise which has expired anyway —
//! and a test immediately caught what that costs: a volunteer that was *mid-unit* when the
//! coordinator died comes back with a perfectly good answer and is refused `NotLeased`. It did
//! the work; the answer is good; the only evidence it was ever assigned the unit lived in the
//! memory of the process that died.
//!
//! A lease is therefore two things at once, and the restart needs only one of them. It is
//! **evidence** that a worker was given a unit, which `submit_result` checks by membership, and
//! it is a **reservation** holding the unit against other workers, which `lease` checks by
//! expiry. Restoring a lease *already expired* gives back the evidence and none of the
//! reservation: the returning volunteer is recognised, and the unit is available to everybody
//! else that same instant.
//!
//! They are appended without `sync_data`, unlike everything else. Losing a lease record to a
//! crash costs exactly what not recording it at all cost — one refused result — so paying an
//! fsync per lease to avoid that would be paying the most frequent write in the system for the
//! least valuable guarantee. They ride along in the file and are flushed by the next result.
//!
//! **Running disputes, and this is the load-bearing decision in the file.** A dispute is a live
//! interactive protocol with a blocking referee thread, two `Desk` mailboxes and two volunteers
//! mid-replay. None of that can be rebuilt from a file, and the alternative — resume the argument
//! and time out whichever party is no longer there — would **convict an honest volunteer for the
//! coordinator's crash.** That is this project's worst outcome and it must not be reachable by
//! restarting a process.
//!
//! So a unit that was in a dispute is **voided**: its results are discarded, it returns to `Open`,
//! and both parties are free to be given it again. The cost is one unit of recomputation. The
//! parties are recorded in [`Entry::Disputed`] so that when reputation lands, "the coordinator
//! dropped this argument" stays distinguishable from "this worker walked away from one".
//!
//! **A concluded bisection's verdict is still lost**, and that is a real limitation rather than
//! an oversight. It was harmless while no verdict had a consequence; B2 gave verdicts teeth and
//! ADR-0017 connected the second route to them, so the gap is now genuine: a coordinator
//! restarted after convicting a liar has forgotten the conviction.
//!
//! **The re-execution route does survive**, because its finding rides along with an outcome that
//! was already being written down. [`Entry::Settled`] carries the verdict and the names it
//! refuted, and replay charges them again. Closing the same hole for bisection needs an entry
//! written from `Grid::account_for_finished_disputes`, which is a sweep rather than a request
//! path and has no journal in scope — a contained job, and a different one.
//!
//! # The format
//!
//! ```text
//! record  := length:u32le ‖ payload ‖ checksum:8
//! payload := tag:u8 ‖ fields…
//! ```
//!
//! The checksum is the first eight bytes of BLAKE3 over the payload — the same hash the rest of
//! the project commits state with, so nothing new is introduced to compute it. It is there to
//! recognise a torn tail, not to resist an attacker: a hostile local file means the machine the
//! coordinator runs on is already lost, and nothing this file could do would help.

use std::fs::{File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use cairn_runtime::dispute::Party;

use crate::dispute::ReExecution;

/// Bytes of BLAKE3 kept after each record.
///
/// Eight, not thirty-two. This distinguishes "the process died mid-write" from "a complete
/// record", and a torn write is not adversarial — it is the tail of a buffer that never reached
/// the disk. Thirty-two bytes would be four times the overhead for a threat model this file
/// explicitly does not have.
const CHECKSUM: usize = 8;

/// Refuse a record claiming to be longer than this.
///
/// A workload may be up to `validate::Limits::max_module_bytes` (32 MiB) and a record carries one,
/// so the ceiling is that plus room for the framing. Its job is to stop a corrupted length prefix
/// from becoming an allocation: without it, four bytes of garbage ask for four gigabytes.
const LONGEST_RECORD: usize = 64 * 1024 * 1024;

/// Something that happened, in the order it happened.
///
/// These are *facts*, not commands. Each one records a decision the grid already made, so
/// replaying them reconstructs the state without re-making any decision — see
/// [`crate::grid::Grid::restore`] for why re-deciding on startup would be worse than useless.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    /// A workload was admitted. Carries the **submitted** source, not the canonical bytes:
    /// replay puts it back through `Grid::register`, which re-instruments and re-derives the id,
    /// so a change to the instrumentation pass shows up as an id that no longer matches rather
    /// than as a grid quietly running different bytes from the ones volunteers were given.
    Registered {
        /// The name it was registered under, for the printed line.
        name: String,
        /// The bytes as submitted.
        source: Vec<u8>,
    },
    /// A unit was queued.
    Queued {
        /// Which workload it runs.
        workload: String,
        /// The bytes handed to `cairn.input`.
        input: Vec<u8>,
        /// How many agreeing results it needs. Recorded rather than recomputed, so that
        /// restarting with a different `--replicate` cannot change the quorum of a unit
        /// volunteers are already working on.
        quorum: usize,
    },
    /// A unit was handed to a volunteer.
    ///
    /// Restored as an **already-expired** lease: evidence that this worker was assigned this
    /// unit, without reserving the unit for a volunteer that may never come back. See the module
    /// docs for why that distinction is the whole point of recording these.
    Leased {
        /// Which unit.
        unit: usize,
        /// Who it went to.
        worker: String,
    },
    /// A volunteer returned a result.
    Answered {
        /// Which unit.
        unit: usize,
        /// Who answered.
        worker: String,
        /// What they said the answer was.
        output: Vec<u8>,
        /// What they said it cost.
        fuel: Option<u64>,
        /// Whether they declared they can be a party to a dispute.
        bisects: bool,
    },
    /// A unit reached its quorum.
    Accepted {
        /// Which unit.
        unit: usize,
        /// The agreed answer.
        output: Vec<u8>,
    },
    /// A disagreement was settled by the referee re-executing the unit.
    Settled {
        /// Which unit.
        unit: usize,
        /// What the referee concluded.
        verdict: ReExecution,
        /// The volunteers its own execution contradicted, by name.
        ///
        /// **Recorded rather than re-derived, and ADR-0014's rule is why.** The verdict names
        /// *parties*, and turning a party back into a worker means knowing which of the two
        /// submissions came first — a fact this entry does not carry and replay must not guess
        /// at. Writing the names down keeps replay a reading of facts instead of a re-run of the
        /// decision that produced them.
        refuted: Vec<String>,
        /// The answer it decided on, if it reached one.
        output: Option<Vec<u8>>,
    },
    /// A canary came back, and whether it was right.
    ///
    /// The canary unit itself is deliberately not recorded. It is a check rather than science,
    /// minted at lease time and worth nothing once answered — what has to survive a restart is
    /// what it *established about the worker*, because losing that would put every volunteer
    /// back on the stranger's sampling rate and make them all earn their standing again.
    Canary {
        /// Who was checked.
        worker: String,
        /// Whether they gave the answer the coordinator already had.
        passed: bool,
    },
    /// A disagreement went to an interactive dispute.
    ///
    /// Recorded so that a restart can *void* it and say whose argument it dropped. It is
    /// deliberately not enough information to resume one; see the module docs.
    Disputed {
        /// Which unit.
        unit: usize,
        /// The two volunteers who were arguing.
        parties: [String; 2],
    },
}

/// Why a journal could not be read or written.
#[derive(Debug)]
pub enum Error {
    /// The file could not be opened, read, appended to or flushed.
    Io(std::io::Error),
    /// A record's checksum did not match, past the end of the file.
    ///
    /// Not reachable from a torn tail — that is truncated silently, which is the whole point of
    /// the checksum. This means damage in the middle of the file.
    Corrupt {
        /// How many records were read before the bad one.
        after: usize,
    },
    /// A record carried a tag this version does not know.
    Unknown {
        /// The tag byte.
        tag: u8,
        /// How many records were read before it.
        after: usize,
    },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "journal io: {e}"),
            Self::Corrupt { after } => write!(
                f,
                "journal is damaged after {after} records — this is not a torn tail, which is \
                 truncated silently, so the file has been modified or the disk is failing"
            ),
            Self::Unknown { tag, after } => write!(
                f,
                "journal record {after} has tag {tag}, which this build does not know — it was \
                 probably written by a newer coordinator"
            ),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// An append-only record of what the coordinator decided.
#[derive(Debug)]
pub struct Journal {
    file: File,
    path: PathBuf,
}

impl Journal {
    /// Open a journal, replaying whatever is already in it.
    ///
    /// Returns the entries in the order they were written. A file that does not exist is a new
    /// journal and an empty history, which is not an error: the first run of a coordinator has
    /// nothing to recover.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] if the file cannot be opened or read. [`Error::Corrupt`] or
    /// [`Error::Unknown`] if what is in it is not something this build wrote. A **torn final
    /// record is not an error** — it is what a crash looks like, and it is truncated.
    pub fn open(path: &Path) -> Result<(Self, Vec<Entry>), Error> {
        let entries = match File::open(path) {
            Ok(mut file) => {
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes)?;
                replay(&bytes)?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(Error::Io(e)),
        };

        let file = OpenOptions::new().append(true).create(true).open(path)?;
        Ok((
            Self {
                file,
                path: path.to_path_buf(),
            },
            entries,
        ))
    }

    /// Where this journal lives, for the startup line.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one entry, making it durable unless it is a lease.
    ///
    /// `sync_data` on every record, not on a timer. A coordinator that acknowledged a volunteer's
    /// result and then lost it has taken somebody's electricity and thrown the answer away, and
    /// the write is one small append against a rare event — a result arrives once per unit, and a
    /// unit is milliseconds to minutes of work.
    ///
    /// [`Entry::Leased`] is the exception and skips the flush. It is the most frequent write in
    /// the system and the least valuable: losing one to a crash costs a single refused result,
    /// which is exactly what happened before leases were recorded at all.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] if the write or the flush fails.
    pub fn append(&mut self, entry: &Entry) -> Result<(), Error> {
        let payload = encode(entry);
        let mut record = Vec::with_capacity(4 + payload.len() + CHECKSUM);
        record.extend_from_slice(
            &u32::try_from(payload.len())
                .unwrap_or(u32::MAX)
                .to_le_bytes(),
        );
        record.extend_from_slice(&payload);
        record.extend_from_slice(
            blake3::hash(&payload)
                .as_bytes()
                .get(..CHECKSUM)
                .unwrap_or_default(),
        );

        self.file.write_all(&record)?;
        if !matches!(entry, Entry::Leased { .. }) {
            self.file.sync_data()?;
        }
        Ok(())
    }
}

/// Read every complete record, stopping at a torn tail.
fn replay(bytes: &[u8]) -> Result<Vec<Entry>, Error> {
    let mut entries = Vec::new();
    let mut at = 0usize;

    while at + 4 <= bytes.len() {
        let Some(header) = bytes
            .get(at..at + 4)
            .and_then(|four| <[u8; 4]>::try_from(four).ok())
        else {
            break;
        };
        let length = u32::from_le_bytes(header) as usize;
        if length > LONGEST_RECORD {
            return Err(Error::Corrupt {
                after: entries.len(),
            });
        }

        // Not enough bytes for the record this header claims: the process died mid-write. That
        // is the ordinary end of a crashed journal, not damage, so stop here and keep what was
        // complete.
        let Some(payload) = bytes.get(at + 4..at + 4 + length) else {
            break;
        };
        let Some(checksum) = bytes.get(at + 4 + length..at + 4 + length + CHECKSUM) else {
            break;
        };

        if blake3::hash(payload).as_bytes().get(..CHECKSUM) != Some(checksum) {
            // A complete-looking record whose bytes do not hash to their checksum. If it is the
            // last one, the length prefix reached the disk and the payload did not — still a
            // torn tail. If anything follows it, the file is damaged and saying so is better
            // than silently dropping the rest of somebody's grid.
            if at + 4 + length + CHECKSUM >= bytes.len() {
                break;
            }
            return Err(Error::Corrupt {
                after: entries.len(),
            });
        }

        entries.push(decode(payload, entries.len())?);
        at += 4 + length + CHECKSUM;
    }

    Ok(entries)
}

// --- encoding -------------------------------------------------------------------------------
//
// Hand-rolled for the same reason `runtime/src/wire.rs` is: a length prefix and a byte string is
// the whole vocabulary, and reaching for a serialisation framework to express it would cost a
// derive macro and a dependency to save the twenty lines below.

const TAG_REGISTERED: u8 = 1;
const TAG_QUEUED: u8 = 2;
const TAG_ANSWERED: u8 = 3;
const TAG_ACCEPTED: u8 = 4;
const TAG_SETTLED: u8 = 5;
const TAG_DISPUTED: u8 = 6;
const TAG_LEASED: u8 = 7;
const TAG_CANARY: u8 = 8;

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&u32::try_from(bytes.len()).unwrap_or(u32::MAX).to_le_bytes());
    out.extend_from_slice(bytes);
}

fn put_usize(out: &mut Vec<u8>, value: usize) {
    out.extend_from_slice(&(value as u64).to_le_bytes());
}

/// A re-execution verdict as one byte.
///
/// These numbers are a wire format: changing what an existing one means silently rewrites
/// history, so add rather than renumber.
const fn verdict_tag(verdict: ReExecution) -> u8 {
    match verdict {
        ReExecution::Refuted {
            wrong: Party::First,
        } => 0,
        ReExecution::Refuted {
            wrong: Party::Second,
        } => 1,
        ReExecution::BothRefuted => 2,
        ReExecution::NoAnswer => 3,
        ReExecution::NotADisagreement => 4,
    }
}

/// The inverse, refusing anything it does not recognise.
///
/// **`None` is what makes a journal from before ADR-0017 fail loudly.** That format put a
/// length-prefixed sentence where this byte now sits, so the first byte of an old record's
/// verdict is a string length — never one of these five in practice, and a corrupt-file error is
/// the right answer either way. Silently misreading an old journal would restore a coordinator
/// with invented verdicts, which is worse than refusing to start.
const fn verdict_from_tag(tag: u8) -> Option<ReExecution> {
    match tag {
        0 => Some(ReExecution::Refuted {
            wrong: Party::First,
        }),
        1 => Some(ReExecution::Refuted {
            wrong: Party::Second,
        }),
        2 => Some(ReExecution::BothRefuted),
        3 => Some(ReExecution::NoAnswer),
        4 => Some(ReExecution::NotADisagreement),
        _ => None,
    }
}

fn encode(entry: &Entry) -> Vec<u8> {
    let mut out = Vec::new();
    match entry {
        Entry::Registered { name, source } => {
            out.push(TAG_REGISTERED);
            put_bytes(&mut out, name.as_bytes());
            put_bytes(&mut out, source);
        }
        Entry::Queued {
            workload,
            input,
            quorum,
        } => {
            out.push(TAG_QUEUED);
            put_bytes(&mut out, workload.as_bytes());
            put_bytes(&mut out, input);
            put_usize(&mut out, *quorum);
        }
        Entry::Answered {
            unit,
            worker,
            output,
            fuel,
            bisects,
        } => {
            out.push(TAG_ANSWERED);
            put_usize(&mut out, *unit);
            put_bytes(&mut out, worker.as_bytes());
            put_bytes(&mut out, output);
            out.push(u8::from(fuel.is_some()));
            out.extend_from_slice(&fuel.unwrap_or(0).to_le_bytes());
            out.push(u8::from(*bisects));
        }
        Entry::Accepted { unit, output } => {
            out.push(TAG_ACCEPTED);
            put_usize(&mut out, *unit);
            put_bytes(&mut out, output);
        }
        Entry::Settled {
            unit,
            verdict,
            refuted,
            output,
        } => {
            out.push(TAG_SETTLED);
            put_usize(&mut out, *unit);
            out.push(verdict_tag(*verdict));
            put_usize(&mut out, refuted.len());
            for worker in refuted {
                put_bytes(&mut out, worker.as_bytes());
            }
            out.push(u8::from(output.is_some()));
            put_bytes(&mut out, output.as_deref().unwrap_or_default());
        }
        Entry::Canary { worker, passed } => {
            out.push(TAG_CANARY);
            put_bytes(&mut out, worker.as_bytes());
            out.push(u8::from(*passed));
        }
        Entry::Leased { unit, worker } => {
            out.push(TAG_LEASED);
            put_usize(&mut out, *unit);
            put_bytes(&mut out, worker.as_bytes());
        }
        Entry::Disputed { unit, parties } => {
            out.push(TAG_DISPUTED);
            put_usize(&mut out, *unit);
            put_bytes(&mut out, parties[0].as_bytes());
            put_bytes(&mut out, parties[1].as_bytes());
        }
    }
    out
}

/// A cursor over one record's payload. Every read is bounds-checked; nothing panics.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn byte(&mut self) -> Option<u8> {
        let byte = *self.bytes.get(self.at)?;
        self.at += 1;
        Some(byte)
    }

    fn u64(&mut self) -> Option<u64> {
        let slice = self.bytes.get(self.at..self.at + 8)?;
        self.at += 8;
        Some(u64::from_le_bytes(slice.try_into().ok()?))
    }

    fn usize(&mut self) -> Option<usize> {
        usize::try_from(self.u64()?).ok()
    }

    fn bytes(&mut self) -> Option<&'a [u8]> {
        let header = self.bytes.get(self.at..self.at + 4)?;
        let length = u32::from_le_bytes(header.try_into().ok()?) as usize;
        let slice = self.bytes.get(self.at + 4..self.at + 4 + length)?;
        self.at += 4 + length;
        Some(slice)
    }

    fn text(&mut self) -> Option<String> {
        Some(String::from_utf8_lossy(self.bytes()?).into_owned())
    }
}

fn decode(payload: &[u8], index: usize) -> Result<Entry, Error> {
    let mut reader = Reader {
        bytes: payload,
        at: 1,
    };
    let corrupt = || Error::Corrupt { after: index };

    let entry = match *payload.first().ok_or_else(corrupt)? {
        TAG_REGISTERED => Entry::Registered {
            name: reader.text().ok_or_else(corrupt)?,
            source: reader.bytes().ok_or_else(corrupt)?.to_vec(),
        },
        TAG_QUEUED => Entry::Queued {
            workload: reader.text().ok_or_else(corrupt)?,
            input: reader.bytes().ok_or_else(corrupt)?.to_vec(),
            quorum: reader.usize().ok_or_else(corrupt)?,
        },
        TAG_ANSWERED => {
            let unit = reader.usize().ok_or_else(corrupt)?;
            let worker = reader.text().ok_or_else(corrupt)?;
            let output = reader.bytes().ok_or_else(corrupt)?.to_vec();
            let has_fuel = reader.byte().ok_or_else(corrupt)? != 0;
            let fuel = reader.u64().ok_or_else(corrupt)?;
            Entry::Answered {
                unit,
                worker,
                output,
                fuel: has_fuel.then_some(fuel),
                bisects: reader.byte().ok_or_else(corrupt)? != 0,
            }
        }
        TAG_ACCEPTED => Entry::Accepted {
            unit: reader.usize().ok_or_else(corrupt)?,
            output: reader.bytes().ok_or_else(corrupt)?.to_vec(),
        },
        TAG_SETTLED => {
            let unit = reader.usize().ok_or_else(corrupt)?;
            let verdict =
                verdict_from_tag(reader.byte().ok_or_else(corrupt)?).ok_or_else(corrupt)?;
            let count = reader.usize().ok_or_else(corrupt)?;
            // A count is a length prefix like any other and gets the same treatment: two parties
            // is the most a dispute has, so anything larger is a corrupted byte asking for an
            // allocation rather than a record.
            if count > 2 {
                return Err(corrupt());
            }
            let mut refuted = Vec::with_capacity(count);
            for _ in 0..count {
                refuted.push(reader.text().ok_or_else(corrupt)?);
            }
            let has_output = reader.byte().ok_or_else(corrupt)? != 0;
            let output = reader.bytes().ok_or_else(corrupt)?.to_vec();
            Entry::Settled {
                unit,
                verdict,
                refuted,
                output: has_output.then_some(output),
            }
        }
        TAG_CANARY => Entry::Canary {
            worker: reader.text().ok_or_else(corrupt)?,
            passed: reader.byte().ok_or_else(corrupt)? != 0,
        },
        TAG_LEASED => Entry::Leased {
            unit: reader.usize().ok_or_else(corrupt)?,
            worker: reader.text().ok_or_else(corrupt)?,
        },
        TAG_DISPUTED => Entry::Disputed {
            unit: reader.usize().ok_or_else(corrupt)?,
            parties: [
                reader.text().ok_or_else(corrupt)?,
                reader.text().ok_or_else(corrupt)?,
            ],
        },
        tag => return Err(Error::Unknown { tag, after: index }),
    };
    Ok(entry)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use super::*;

    /// A path to write a throwaway journal to.
    ///
    /// `std::env::temp_dir` rather than `CARGO_TARGET_TMPDIR`, which cargo defines for
    /// integration tests and not for a lib's own. Named per test, because the harness runs them
    /// on separate threads and two tests sharing a file would fail in a way that looked like a
    /// bug in the journal.
    fn scratch(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("cairn-journal-test-{name}"))
    }

    #[test]
    fn a_settled_record_from_before_the_verdict_had_a_type_is_refused_rather_than_misread() {
        // The one hazard in changing a wire format, and the reason `verdict_from_tag` returns an
        // `Option` instead of falling back to something harmless-looking. Until ADR-0017 a
        // settled record carried a length-prefixed sentence where the verdict byte now sits, so
        // this reader would take the first byte of a string length as a verdict. Restoring a
        // coordinator with an invented verdict is worse than refusing to start.
        //
        // Written as bytes rather than by the old encoder, which is gone. What matters is the
        // shape: a plausible byte in the verdict position that this reader does not know.
        let mut payload = vec![TAG_SETTLED];
        payload.extend_from_slice(&0_u64.to_le_bytes());
        payload.push(b'\x2a');

        assert!(
            decode(&payload, 0).is_err(),
            "an unrecognised verdict tag was accepted"
        );

        // And every tag it *does* know survives the trip, so the guard above is rejecting the
        // right thing rather than everything.
        for verdict in [
            ReExecution::Refuted {
                wrong: Party::First,
            },
            ReExecution::Refuted {
                wrong: Party::Second,
            },
            ReExecution::BothRefuted,
            ReExecution::NoAnswer,
            ReExecution::NotADisagreement,
        ] {
            assert_eq!(verdict_from_tag(verdict_tag(verdict)), Some(verdict));
        }
    }

    fn every_shape() -> Vec<Entry> {
        vec![
            Entry::Registered {
                name: "sum-of-squares.wat".to_owned(),
                source: vec![0, 97, 115, 109],
            },
            Entry::Queued {
                workload: "abc123".to_owned(),
                input: b"abcde".to_vec(),
                quorum: 2,
            },
            Entry::Answered {
                unit: 7,
                worker: "alice".to_owned(),
                output: vec![1, 2, 3],
                fuel: Some(850_022),
                bisects: true,
            },
            Entry::Answered {
                unit: 7,
                worker: "bob".to_owned(),
                output: Vec::new(),
                fuel: None,
                bisects: false,
            },
            Entry::Accepted {
                unit: 7,
                output: vec![9],
            },
            Entry::Settled {
                unit: 8,
                verdict: ReExecution::Refuted {
                    wrong: Party::Second,
                },
                refuted: vec!["bob".to_owned()],
                output: Some(vec![4, 5]),
            },
            Entry::Settled {
                unit: 9,
                verdict: ReExecution::NoAnswer,
                refuted: Vec::new(),
                output: None,
            },
            // Both refuted, so both names ride along. The one shape a caller that only handled
            // `Refuted` would silently shorten.
            Entry::Settled {
                unit: 10,
                verdict: ReExecution::BothRefuted,
                refuted: vec!["bob".to_owned(), "carol".to_owned()],
                output: Some(vec![7]),
            },
            Entry::Leased {
                unit: 11,
                worker: "carol".to_owned(),
            },
            Entry::Canary {
                worker: "carol".to_owned(),
                passed: true,
            },
            Entry::Canary {
                worker: "mallory".to_owned(),
                passed: false,
            },
            Entry::Disputed {
                unit: 10,
                parties: ["honest".to_owned(), "liar".to_owned()],
            },
        ]
    }

    fn written(entries: &[Entry]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for entry in entries {
            let payload = encode(entry);
            bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            bytes.extend_from_slice(&payload);
            bytes.extend_from_slice(&blake3::hash(&payload).as_bytes()[..CHECKSUM]);
        }
        bytes
    }

    #[test]
    fn every_entry_survives_a_round_trip() {
        let entries = every_shape();
        assert_eq!(replay(&written(&entries)).unwrap(), entries);
    }

    #[test]
    fn a_torn_final_record_is_the_end_of_the_file_rather_than_an_error() {
        // What a crash looks like. Every prefix of a valid journal must read back as the entries
        // that completed — never an error, and never a half-decoded entry.
        let entries = every_shape();
        let bytes = written(&entries);

        for cut in 0..bytes.len() {
            let read = replay(&bytes[..cut]).unwrap_or_else(|e| {
                panic!(
                    "a journal truncated at {cut} of {} bytes failed: {e}",
                    bytes.len()
                )
            });
            assert!(
                read.len() <= entries.len(),
                "truncation invented an entry at {cut}"
            );
            assert_eq!(
                read[..],
                entries[..read.len()],
                "truncating at {cut} changed an entry that was complete"
            );
        }
    }

    #[test]
    fn damage_in_the_middle_is_refused_rather_than_silently_dropping_the_rest() {
        // The one case that must NOT be treated as a torn tail. A flipped bit early in the file
        // would otherwise silently discard every unit after it, and a coordinator that came up
        // having quietly forgotten half its grid is worse than one that refuses to come up.
        let entries = every_shape();
        let mut bytes = written(&entries);
        bytes[6] ^= 0xff;

        assert!(
            matches!(replay(&bytes), Err(Error::Corrupt { .. })),
            "damage before the last record must be an error"
        );
    }

    #[test]
    fn a_corrupt_length_prefix_does_not_become_an_allocation() {
        // Four bytes of garbage must not be read as "allocate four gigabytes". The same hazard
        // `wire.rs` guards, for the same reason, in a file a stranger does not write — because
        // the day somebody points this at a shared volume is not the day to discover it.
        let mut bytes = written(&every_shape());
        bytes[0..4].copy_from_slice(&u32::MAX.to_le_bytes());

        assert!(matches!(replay(&bytes), Err(Error::Corrupt { .. })));
    }

    #[test]
    fn an_unknown_tag_says_so_rather_than_guessing() {
        let mut bytes = Vec::new();
        let payload = vec![200u8, 0, 0, 0, 0];
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&payload);
        bytes.extend_from_slice(&blake3::hash(&payload).as_bytes()[..CHECKSUM]);

        assert!(matches!(
            replay(&bytes),
            Err(Error::Unknown { tag: 200, after: 0 })
        ));
    }

    #[test]
    fn an_absent_journal_is_an_empty_history_rather_than_a_failure() {
        let path = scratch("absent-journal.log");
        let _ = std::fs::remove_file(&path);

        let (_journal, entries) = Journal::open(&path).expect("a new journal is not an error");
        assert!(entries.is_empty());
        assert!(path.exists(), "opening should have created the file");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn what_was_appended_is_what_is_read_back() {
        let path = scratch("round-trip.log");
        let _ = std::fs::remove_file(&path);

        {
            let (mut journal, _) = Journal::open(&path).unwrap();
            for entry in every_shape() {
                journal.append(&entry).unwrap();
            }
        }

        let (_journal, entries) = Journal::open(&path).unwrap();
        assert_eq!(entries, every_shape());
        let _ = std::fs::remove_file(&path);
    }
}
