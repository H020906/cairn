# ADR-0014 — The coordinator keeps a log, not a database

- **Status:** Accepted
- **Date:** 2026-08-10
- **Amends:** [ADR-0002](0002-language-boundaries.md) and `ARCHITECTURE.md`, which say PostgreSQL and SQLite

## Context

Until now the coordinator's state died with the process. Every unit, every result and every
verdict lived in memory, and stopping the process meant a grid of volunteers had done work that
was thrown away. `ARCHITECTURE.md` says the system of record is PostgreSQL with SQLite in the
native worker, and the roadmap item for this work said "Persistence (SQLite)".

Writing it revealed that the coordinator has no use for a database, and the reason is specific
rather than a matter of taste.

**There are no queries.** Every read in `grid.rs` is already a linear scan of an in-memory `Vec`:
`lease` walks the units, `dispute_for` walks the disputes, `status` renders all of them. The whole
state fits in memory and is designed to. What persistence has to provide here is not "where the
data lives" — it is "how the memory is rebuilt after a restart". That is a log.

**The dependency rule.** This project admits a dependency when it does something the standard
library cannot: `tiny_http` parses requests from strangers, `wasmparser` implements a
specification, `blake3` is a hash nobody should write. Durably appending a record and replaying a
file is not in that category. `rusqlite` would bring a bundled C amalgamation into the one
component that decides who is convicted of cheating, to buy indexes over nothing and transactions
over a single writer.

The honest counter-argument, which deserves stating rather than caricaturing: **SQLite would bring
somebody else's crash-safety testing**, and "I can write durable storage myself" is a famous way
to be wrong. What answers it is the shape of the failure. Writes here are sequential appends by
one writer, so a crash can tear only the **last** record, and recovery is "stop at the first
record that fails its checksum". That is small enough to be tested exhaustively — and it is:
every prefix of a valid journal is replayed and required to yield exactly the entries that were
complete.

## Decision

**An append-only journal, in `coordinator/src/journal.rs`, replayed through `Grid::restore`.**

```text
record  := length:u32le ‖ payload ‖ checksum:8
payload := tag:u8 ‖ fields…
```

The checksum is the first eight bytes of BLAKE3 over the payload. It exists to recognise a torn
tail, not to resist an attacker: a hostile local file means the machine is already lost.

`sync_data` after every record **except leases** — see below. A coordinator that acknowledged a
result and then lost it has taken somebody's electricity and thrown the answer away.

**Replay records facts; it does not re-make decisions.** The tempting implementation puts every
entry back through `register`, `submit` and `submit_result`, so that a restored grid is provably
reachable by ordinary operation. It is wrong here, concretely: `submit_result` on the second
disagreeing result **opens a live dispute** — a referee thread and a patience timer, against
volunteers who are not connected yet. Replay would start a fresh argument for every dispute the
coordinator had ever had and lose all of them by timeout, **convicting honest volunteers on
startup**. Registration is the one exception and does go back through the live path, so that a
change to the instrumentation pass surfaces as a unit id that no longer matches rather than as a
grid quietly serving different bytes than its volunteers were given.

**A unit that was mid-argument is voided.** Its results are discarded, it returns to `Open`, and
both parties may be given it again. A dispute is a live interactive protocol with a blocking
referee, two mailboxes and two volunteers mid-replay; it cannot be rebuilt from a file. The
alternative — resume, and time out whichever party did not come back — convicts an honest
volunteer for the coordinator's crash, which is this project's worst outcome and must not be
reachable by restarting a process. The parties are recorded so that when reputation lands, "the
coordinator dropped this argument" stays distinguishable from "this worker walked away from one".

### The finding: a lease is two things, and only one of them survives

Leases were originally left out, on the reasoning that a lease is a promise which has expired by
the time a restart happens. A test caught what that costs immediately:

> A volunteer that was **mid-unit** when the coordinator died comes back with a perfectly good
> answer and is refused `NotLeased`.

It did the work. The answer is good. The only evidence it was ever assigned the unit lived in the
memory of the process that died. Throwing that away is precisely the loss this whole change exists
to prevent.

So a lease is **evidence** that a worker was given a unit, which `submit_result` reads by
membership, and a **reservation** holding the unit against others, which `lease` reads by expiry.
A restart needs the first and must not restore the second — a reservation for a volunteer who may
never come back would stall every in-flight unit for a lease timeout.

Restoring a lease *already expired* gives exactly that split. Making it work required one change
in `grid.rs` that is worth knowing about: **expiry is now applied where it is read, instead of by
deleting.** `lease` used to prune expired entries, which destroyed the evidence a moment later;
it now counts the live ones. Leases are also journalled **without** `sync_data`, because losing
one to a crash costs a single refused result — exactly what not recording them at all cost — and
they are the most frequent write in the system.

## Consequences

Measured end to end: 60 units, one four-job volunteer, the coordinator killed outright partway
through and restarted against the same journal.

| | |
|---|---:|
| units accepted when the coordinator was killed | 16 |
| accepted units recovered on restart | 16 |
| units executed after the restart | 44 |
| **total units executed, across both lives** | **60** |
| a perfect run | 60 |
| journal size | 17.2 kB |

**Nothing lost and nothing repeated**, which is exactly what the work was for. About 287 bytes per
unit, including the workload source.

What this costs, stated rather than discovered later:

- **The journal never gets smaller.** There is no compaction and no snapshot. A grid that runs a
  million units carries a ~290 MB file and replays all of it at startup. That is fine for
  everything this project can currently do and it is a real ceiling, not a theoretical one.
- **One writer, one process.** No second coordinator can share this file, which is the thing a
  real deployment would need and the reason `ARCHITECTURE.md` wanted PostgreSQL. This ADR does not
  refute that; it says the single-node coordinator does not need it yet.
- **No directory fsync.** A record's *contents* are durable when `append` returns; the file's
  *creation* is at the filesystem's discretion on the first write. That matters only for a crash
  in the first moments of a brand-new journal.
- **A concluded dispute's verdict is lost.** It costs nothing today because no verdict has a
  consequence — `grid.rs` says plainly there are no penalties and no reputation. When B2 gives a
  verdict teeth, verdicts become worth persisting and this file will need an entry for them.
- **A liar could in principle escape a conviction by waiting for a restart.** It cannot cause one,
  and there is nothing yet to escape, but the incentive appears the moment reputation does.

## Alternatives considered

**SQLite via `rusqlite`.** The documented plan. Rejected above: no queries to serve, a large C
dependency in the component that convicts people, and a failure mode small enough to test
exhaustively without it. Worth revisiting if the coordinator ever grows a dashboard that queries
history rather than rendering current state.

**PostgreSQL, as `ARCHITECTURE.md` describes.** Not rejected — deferred, and for the reason
ADR-0010 gave about Java: it buys things a single-process coordinator has no use for yet, and
carrying a service to run alongside it costs every contributor who wants to try the project. It is
what a multi-coordinator deployment would need, and that deployment does not exist.

**Snapshot the whole grid periodically instead of logging.** Simpler to write and worse in the
case that matters: a crash loses everything since the last snapshot, and the results people
donated between snapshots are exactly what a restart is supposed to keep. The log's append is also
cheaper than serialising the whole state.

**Persist and resume disputes.** Rejected in the strongest terms available: the reconstruction is
not possible (two mailboxes, a blocking referee and two volunteers mid-replay), and the closest
approximation convicts an honest volunteer for a crash.

**Do not journal leases.** Tried first, and refuted by a test rather than by argument. See above.
