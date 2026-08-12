# ADR-0018 — Admit reference types for their encoding, refuse them as values

- **Status:** Accepted
- **Date:** 2026-08-12

## Context

The plan for phase C called this item *"widen the admitted instruction set to what compilers
emit"*, on the assumption that the work would be adding instructions the interpreter did not yet
implement — SIMD, tail calls, and so on. The first measurement said otherwise.

Compile any Rust program containing a trait object or a function pointer to
`wasm32-unknown-unknown` and hand the result to `cairn-worker prepare`:

```
error: indirect.wasm is not an admissible Cairn workload:
       not a valid Cairn module: zero byte expected (at offset 0x116)
```

At offset `0x110`:

```
11 80 80 80 80 00 80 80 80 80 00      call_indirect (type 0) (table 0)
```

`0x11` is `call_indirect`. The first `80 80 80 80 00` is the type index, and the second is **the
table index**. Both are LEB128 encodings of zero, padded to five bytes so that a linker can
relocate them without moving anything.

The base specification does not allow that. It requires a single `0x00` byte in the table-index
position, and the multi-byte form became legal only with the **reference-types** proposal. Cairn
refused reference types, so Cairn refused the module.

**Nothing else about the module was objectionable.** One table, table index zero, no reference in
any value position, and an interpreter that already implements `call_indirect` completely,
signature check included. The refusal was about how a zero is spelled.

**The reach of that is most of the compiler output anybody would write.** A trait object, a
function pointer, a `dyn` anything, a closure stored behind a pointer — each one is enough. What
hid it is that `math-probe`, the only compiled workload in the repository before this, contains no
indirect call at all: `cairn-math` is statically resolved arithmetic from end to end. The one test
that was supposed to prove Cairn admits stock toolchain output happened to use the one shape that
does not exercise a table.

The blanket refusal was not arbitrary. [ADR-0005](0005-the-fast-path-cannot-snapshot.md) and
[ADR-0003](0003-determinism-constraints.md) establish that a state commitment must cover every
value the machine holds, and a `funcref` or `externref` has no host-independent representation —
`crate::state::Value` has no case for one and could not have. That reasoning is about references
as **values**, and it is still correct. It says nothing about how a zero is written down.

## Decision

**Enable `REFERENCE_TYPES` in `admitted_features`, and move what the feature gate used to do into
the structural pass — where it can be stated precisely.**

Four new refusals in `validate.rs`, each naming the property it protects:

- **`ReferenceValue`** — a reference appearing as a parameter, a result, a local, or a global's
  type. This is the ADR-0005 property, kept intact: no `funcref` or `externref` can reach the
  operand stack, so `state::Value` still needs no case for one.
- **`MultipleTables`** — more than one table. The interpreter resolves `call_indirect` against a
  single table and **ignores the instruction's table index**. A second table would be indexed
  correctly by every other engine and incorrectly by Cairn, which is a consensus divergence rather
  than a missing feature.
- **`NonFunctionTable`** — a table of anything but functions. The table is the one place a
  reference may live, and only in the narrow sense that it is `call_indirect`'s target space; the
  interpreter stores function indices there, not references.
- **`ReferenceInstruction`** — `ref.null`, `ref.func`, `ref.is_null`, and every `table.*`. Refused
  **at the gate rather than left to trap in the interpreter**, and that distinction is the whole
  point: every other engine executes these instructions happily, so a module carrying one would
  run to completion for a volunteer and trap for the referee, and the referee would convict the
  volunteer.

The instruction list is enumerated rather than matched on a name prefix, and the feature gate is
what makes that safe: an operator can only appear in an admitted module if the proposal defining
it is in `admitted_features`, so the list covers exactly one proposal rather than every
instruction `wasmparser` knows.

## Consequences

**What it buys.** Workloads can use trait objects, function pointers, and dynamic dispatch — which
is to say, workloads can be written the way people write programs. `workloads/rust/dispatch-probe`
is deliberately built out of those constructs, goes through the gate, and is required to produce
identical bytes and identical fuel on Cairn's interpreter, wasmi, and wasmtime.

**What did not have to change, and it is worth knowing why.** The instrumentation pass inserts
functions, which shifts every function index in the module — including the ones inside element
segments, which is how a table's contents are named. Nothing had to be added for that, because
`canon.rs` overrides `Reencode::function_index` rather than hand-rolling the remapping, and
`wasm-encoder` then rewrites calls, exports **and element segments** through it. A hand-written
index fixup would almost certainly have missed the element segments, and the failure would have
been a table pointing at the wrong functions — a wrong answer rather than an error.

**What survives from the blanket refusal.** Both properties it was protecting:

- No reference reaches a value position, so `StateCommitment` is unchanged.
- Every table-mutating instruction is refused, so the table is fixed once its element segments are
  applied — which is exactly what lets `StateCommitment` leave the table out. The warning on
  `install_table` becomes load-bearing rather than advisory: **if table mutation is ever
  implemented, the table joins the commitment in the same change.**

**What is narrower than it reads.** `externref` needs the GC feature in the current `wasmparser`,
so the feature gate refuses it before the structural pass sees it, and `NonFunctionTable` is
currently unreachable. It is kept for the reason `Rejection`'s own documentation gives about
`Memory64` and `CustomPageSize`: the structural pass has to stand on its own, or it fails open at
the exact moment the allowlist widens again.

**What this says about the rest of phase C.** The plan assumed the work was implementing
instructions. The first thing it found was a gate that refused a legal spelling of a number the
interpreter already handled. The remaining candidates — SIMD, tail calls, `extended_const` — should
each be measured the same way before being estimated: compile something real, see what is refused,
and find out whether the refusal is about semantics or about notation.

**What it does not change.** Nothing about ADR-0016's constraint. No instruction admitted here
introduces a host-computed numeric result, and `fma` remains absent.

## Alternatives considered

**Rewrite the immediate before validating.** Normalise the padded LEB to a single zero byte and
carry on. Rejected twice over: admission would then be deciding on bytes the author did not
submit, and the unit id is the hash of the canonical module derived from what *was* submitted, so
a workload's identity would depend on a rewrite performed before anybody agreed to it.

**Ask workload authors to pass `-C target-feature=-reference-types`.** It works, and it is the
kind of instruction that turns a workload contract into a folklore collection. It also fails the
stated test for this phase: somebody who is not me should be able to compile a workload, and a
flag whose absence produces `zero byte expected (at offset 0x116)` is not discoverable.

**Enable the feature and rely on the interpreter's `Trap::Unsupported`.** Cheap, and wrong in the
most dangerous available way. `Unsupported` is an internal invariant failure, raised by Cairn and
by nothing else; a module using `table.set` would complete on the volunteer's engine and trap for
the referee. That is the exact shape of `br 0` at function scope (`3b2ebcb`) — Cairn stops, the
other engine continues, arbitration convicts the honest party. The gate has to refuse it.

**Implement the table instructions instead of refusing them.** Defensible, and a genuine option
later. Rejected for now because it is a different and larger change: the table would have to enter
`StateCommitment`, which touches the commitment format, the witness, and adjudication. The
instructions that were blocking real workloads were none of these — they were `call_indirect`,
which already worked.
