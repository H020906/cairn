//! Decides which WebAssembly modules Cairn is willing to run.
//!
//! Two rules govern this module, and they are different in kind.
//!
//! **Rule one: nothing nondeterministic.** Threads, relaxed SIMD and stack switching produce
//! host-dependent results by construction. A workload using them would make honest workers
//! disagree, and dispute arbitration would convict one of them at random.
//!
//! **Rule two: nothing we cannot commit to.** The execution trace is a Merkle commitment over
//! linear memory, globals and the value stack. Every value in that state must be hashable in a
//! host-independent way. `externref` and `funcref` are opaque host references — there is no
//! meaningful way to hash one, so a module that can place one on the stack cannot be committed to
//! at all. This is a structural reason rather than a scheduling one, and it is enforced by
//! [`Rejection::ReferenceValue`] and [`Rejection::ReferenceInstruction`] rather than by the
//! feature gate: see [`admitted_features`] for why the reference-types *proposal* is admitted
//! even though a reference *value* is not.
//!
//! # The allowlist is the interpreter's coverage
//!
//! The set of proposals admitted here is deliberately identical to the set the instrumented
//! interpreter implements. Keeping them equal means Cairn can never accept a module it would
//! later be unable to arbitrate — the failure mode where a dispute arrives and the coordinator
//! discovers it cannot replay the disputed instruction. When the interpreter gains an
//! instruction family, this list grows in the same commit, and not before.
//!
//! **Where a proposal is admitted for part of itself, the structural pass draws the rest of the
//! line** — and it must refuse at the gate rather than leave the interpreter to trap. A trap is
//! Cairn's own invariant failure and no other engine raises it, so a module Cairn traps on and
//! wasmtime completes is a dispute that convicts the honest volunteer. ADR-0018 is that argument
//! written out; `3b2ebcb` is what it looks like when it actually happens.
//!
//! # A note on floating point
//!
//! `wasmparser` offers a `FLOATS` feature gate specifically because NaN bit patterns vary
//! between hosts. Cairn keeps floating point **enabled** — the scientific workloads it exists
//! to serve are almost entirely floating point — and handles NaN divergence in the
//! instrumentation pass instead, by canonicalizing after every operation that can produce one.
//! See `canon` and ADR-0003.

use std::fmt;

use wasmparser::{
    CompositeInnerType, ExternalKind, MemoryType, Parser, Payload, TypeRef, ValType, Validator,
    WasmFeatures,
};

/// The import module name reserved for Cairn's host interface.
///
/// A workload may import from this module and from nowhere else. There is no filesystem, no
/// clock, no network and no entropy — not restricted, simply absent.
pub const HOST_MODULE: &str = "cairn";

/// Host function: `input(ptr: i32, len: i32) -> i32`.
///
/// Copies up to `len` bytes of the work unit's input to `ptr` and returns the input's true
/// length, so a workload can size its buffer with a zero-length probe.
pub const HOST_INPUT: &str = "input";

/// Host function: `output(ptr: i32, len: i32)`.
///
/// Records `len` bytes at `ptr` as the work unit's result. The last call wins.
pub const HOST_OUTPUT: &str = "output";

/// Host function: `charge(instructions: i32)`.
///
/// The fuel-metering and snapshot hook. **Injected by the instrumentation pass, never written
/// by a workload author** — a submitted module importing this name is rejected, because
/// importing it directly would let a workload lie about how much it had executed.
pub const HOST_CHARGE: &str = "charge";

/// The global a metered module exports its instruction count under: `cairn_fuel`, an `i64`.
///
/// **Injected by the instrumentation pass, never written by a workload author.** It is the
/// counterpart to [`HOST_CHARGE`] for engines that cannot afford a host call per basic block:
/// the module accumulates into the global, and whoever ran it reads the total afterwards. A
/// submitted module exporting this name is rejected for the same reason importing `charge` is
/// — it is the count of its own execution, and a workload must not be able to write it.
pub const FUEL_EXPORT: &str = "cairn_fuel";

/// The function a work unit must export as its entry point.
pub const ENTRY_POINT: &str = "cairn_run";

/// The name the module's linear memory must be exported under.
pub const MEMORY_EXPORT: &str = "memory";

/// The WebAssembly proposals Cairn admits.
///
/// Everything absent from this set is rejected, in every case for one of the two reasons in
/// the module documentation. The specific exclusions worth knowing about:
///
/// | Excluded | Why |
/// |---|---|
/// | threads, shared-everything-threads, stack-switching | nondeterministic by construction |
/// | relaxed SIMD | nondeterministic by explicit design of the proposal |
/// | GC, function references | host-opaque values cannot enter a state commitment |
/// | custom page sizes, memory control | the memory commitment assumes 64 KiB pages |
/// | multi-memory | one memory means one page tree |
/// | SIMD, tail call, exceptions, memory64, extended const, wide arithmetic | deterministic, but not yet implemented by the interpreter |
/// | component model | not a core module |
///
/// # Reference types are admitted for their *encoding* and refused for their *values*
///
/// This is the one entry that is not a straight yes or no, and the reason is empirical. Compile
/// any Rust program containing a trait object or a function pointer to `wasm32-unknown-unknown`
/// and the `call_indirect` it emits carries its table index as a **padded five-byte LEB128**:
///
/// ```text
/// 11 80 80 80 80 00 80 80 80 80 00      call_indirect (type 0) (table 0)
/// ```
///
/// The base specification requires a single zero byte in that position. The multi-byte form is
/// legal only under the reference-types proposal, so without it `wasmparser` reports
/// `zero byte expected` and the module is refused — **even though the table index is zero, the
/// module declares one table, and no reference ever reaches a value position.** That refusal is
/// about how a zero is spelled, and it excluded most non-trivial compiler output.
///
/// So the feature is enabled and the structural pass takes over what the feature gate used to do,
/// which is the stronger arrangement in any case: it refuses a reference *value* anywhere
/// ([`Rejection::ReferenceValue`]), a second table ([`Rejection::MultipleTables`]), a table of
/// anything but functions ([`Rejection::NonFunctionTable`]), and every instruction that touches a
/// reference or mutates a table ([`Rejection::ReferenceInstruction`]).
///
/// **What survives from the old blanket refusal is the property that mattered.** No `funcref` or
/// `externref` can reach the operand stack, a local, or a global, so [`crate::state::Value`] still
/// needs no case for one. And because every table-mutating instruction is refused, the table is
/// fixed once its element segments are applied — which is what lets `StateCommitment` leave the
/// table out.
#[must_use]
pub fn admitted_features() -> WasmFeatures {
    WasmFeatures::MUTABLE_GLOBAL
        | WasmFeatures::SIGN_EXTENSION
        | WasmFeatures::SATURATING_FLOAT_TO_INT
        | WasmFeatures::MULTI_VALUE
        | WasmFeatures::BULK_MEMORY
        | WasmFeatures::FLOATS
        | WasmFeatures::REFERENCE_TYPES
}

/// Whether a value type is a reference, and therefore cannot be committed to.
const fn is_reference(ty: ValType) -> bool {
    matches!(ty, ValType::Ref(_))
}

/// Whether an instruction reads, writes or creates a reference.
///
/// **Enumerated rather than pattern-matched on a name prefix, and the feature gate is what makes
/// that safe.** An operator can only appear in an admitted module if the proposal that defines it
/// is in [`admitted_features`], so this list has to cover exactly one proposal's instructions
/// rather than every instruction `wasmparser` knows. A future proposal that adds another
/// table-mutating instruction cannot reach here without somebody first enabling its feature, and
/// that edit is where the decision belongs.
///
/// `elem.drop` is deliberately absent: it is bulk memory rather than reference types, the
/// interpreter implements it, and dropped segments are part of the state commitment already.
fn touches_a_reference(op: &wasmparser::Operator<'_>) -> bool {
    use wasmparser::Operator as Op;
    match op {
        Op::RefNull { .. }
        | Op::RefIsNull
        | Op::RefFunc { .. }
        | Op::TableGet { .. }
        | Op::TableSet { .. }
        | Op::TableSize { .. }
        | Op::TableGrow { .. }
        | Op::TableFill { .. }
        | Op::TableCopy { .. }
        | Op::TableInit { .. } => true,
        // A `select` carrying an explicit type is only a problem when that type is a reference.
        // The untyped `select` cannot be one: the base specification restricts it to numbers.
        Op::TypedSelect { ty } => is_reference(*ty),
        _ => false,
    }
}

/// Structural ceilings applied on top of the feature gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Largest declared memory, in 64 KiB pages. Bounds the depth of the memory commitment.
    pub max_memory_pages: u32,
    /// Largest accepted module binary, in bytes.
    pub max_module_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            // 4096 pages = 256 MiB, a depth-12 page tree.
            max_memory_pages: 4096,
            // 32 MiB. Work units are distributed to browsers on domestic connections.
            max_module_bytes: 32 * 1024 * 1024,
        }
    }
}

/// Why a module was refused.
///
/// # Overlap with the feature gate
///
/// Several variants here — [`SharedMemory`](Self::SharedMemory),
/// [`Memory64`](Self::Memory64), [`CustomPageSize`](Self::CustomPageSize),
/// [`MultipleMemories`](Self::MultipleMemories) — describe conditions that
/// [`admitted_features`] already makes impossible, so in the current configuration
/// [`validate_submitted`] reports them as [`Invalid`](Self::Invalid) instead. They are kept
/// because the structural pass is written to stand on its own: it must stay correct if the
/// allowlist later admits a proposal, and a structural rule that silently depended on the
/// feature gate would fail open at exactly that moment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejection {
    /// The binary exceeded [`Limits::max_module_bytes`].
    TooLarge {
        /// Size of the submitted binary.
        bytes: usize,
        /// The configured ceiling.
        limit: usize,
    },
    /// `wasmparser` refused the module: malformed, invalid, or using a gated proposal.
    ///
    /// The detail is the underlying message, which names the offending feature.
    Invalid {
        /// The underlying parser or validator message.
        detail: String,
    },
    /// The module declares no linear memory. Work units communicate through memory.
    NoMemory,
    /// More than one memory. One memory means one page tree.
    MultipleMemories,
    /// The memory is shared, which implies threads.
    SharedMemory,
    /// A 64-bit memory. Not yet implemented by the interpreter.
    Memory64,
    /// The memory declares a non-standard page size; the commitment assumes 64 KiB.
    CustomPageSize,
    /// The memory declares no maximum, so its size would depend on the host.
    UnboundedMemory,
    /// The declared maximum exceeds [`Limits::max_memory_pages`].
    MemoryTooLarge {
        /// Declared maximum, in pages.
        pages: u64,
        /// The configured ceiling, in pages.
        limit: u32,
    },
    /// The memory is imported. Cairn supplies no memory; a workload defines its own.
    ImportedMemory,
    /// The module has a start section. Everything must run under the entry point so that all
    /// executed instructions are metered.
    StartSection,
    /// An import from somewhere other than the `cairn` module.
    ForeignImport {
        /// The module the import named.
        module: String,
        /// The import's name.
        name: String,
    },
    /// An import of a name reserved for the instrumentation pass.
    ReservedImport {
        /// The reserved name that was imported.
        name: String,
    },
    /// An export of a name reserved for the instrumentation pass.
    ReservedExport {
        /// The reserved name that was exported.
        name: String,
    },
    /// An import from the `cairn` module that is not part of the host interface.
    UnknownHostFunction {
        /// The name that was imported.
        name: String,
    },
    /// A host function imported with the wrong signature.
    HostSignatureMismatch {
        /// The host function's name.
        name: String,
        /// The signature the module declared, rendered for a human.
        declared: String,
    },
    /// A host import that is not a function — a memory, table or global.
    NonFunctionHostImport {
        /// The name that was imported.
        name: String,
    },
    /// A required export is missing or is of the wrong kind.
    MissingExport {
        /// The export that was expected.
        name: String,
    },
    /// A reference type appears somewhere a value can be, which no state commitment can cover.
    ///
    /// **This is the structural half of admitting the reference-types proposal.** The feature is
    /// enabled because every current toolchain encodes `call_indirect`'s table index as a LEB128
    /// integer, which the proposal permits and the base specification does not — see
    /// [`admitted_features`]. That is an *encoding*, and it is all Cairn wants from the proposal.
    ///
    /// What Cairn cannot have is a reference as a **value**. A `funcref` or `externref` on the
    /// operand stack, in a local, or in a global has no host-independent representation, so
    /// [`crate::state::Value`] cannot hold one and a trace commitment cannot cover one. Admitting
    /// the encoding without this check would admit the values too.
    ReferenceValue {
        /// Where it appeared, for a human: `"a global"`, `"function type 3"`, and so on.
        at: String,
    },
    /// More than one table.
    ///
    /// Multiple tables arrive with reference types, and the interpreter has exactly one — its
    /// `call_indirect` resolves against that one and ignores the table index in the instruction.
    /// A second table would therefore be indexed correctly by every other engine and incorrectly
    /// by Cairn, which is a consensus divergence rather than a missing feature.
    MultipleTables,
    /// A table of something other than functions.
    ///
    /// The table is the only place a reference is allowed to live, because it is not a value
    /// there — it is the target space `call_indirect` selects from, and the interpreter stores
    /// function *indices*. A table of `externref` would be host-opaque storage.
    NonFunctionTable,
    /// An instruction that reads, writes or creates a reference.
    ///
    /// `ref.func`, `ref.null`, `table.get`, `table.set` and the rest. Every one of them either
    /// puts a reference where a value goes or mutates the table, and the table is deliberately
    /// outside the state commitment on the grounds that nothing can change it. **Refused at the
    /// gate rather than left to trap**, because a trap in Cairn's interpreter against an engine
    /// that executes the instruction happily is precisely how an honest volunteer is convicted.
    ReferenceInstruction {
        /// The instruction, as `wasmparser` names it.
        operator: String,
    },
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge { bytes, limit } => {
                write!(f, "module is {bytes} bytes, over the {limit}-byte limit")
            }
            Self::Invalid { detail } => write!(f, "not a valid Cairn module: {detail}"),
            Self::NoMemory => write!(f, "module declares no linear memory"),
            Self::MultipleMemories => {
                write!(
                    f,
                    "module declares more than one memory; exactly one is required"
                )
            }
            Self::SharedMemory => write!(f, "shared memory implies threads, which are forbidden"),
            Self::Memory64 => write!(f, "64-bit memory is not implemented by the interpreter"),
            Self::CustomPageSize => write!(
                f,
                "custom page sizes are forbidden; the memory commitment assumes 64 KiB pages"
            ),
            Self::UnboundedMemory => write!(
                f,
                "memory must declare a maximum, or its size would depend on the host"
            ),
            Self::MemoryTooLarge { pages, limit } => write!(
                f,
                "memory maximum of {pages} pages exceeds the {limit}-page limit"
            ),
            Self::ImportedMemory => {
                write!(f, "memory must be defined by the module, not imported")
            }
            Self::StartSection => write!(
                f,
                "start sections are forbidden; all execution must occur under `{ENTRY_POINT}` so \
                 that every instruction is metered"
            ),
            Self::ForeignImport { module, name } => write!(
                f,
                "import `{module}::{name}` is not permitted; the only importable module is \
                 `{HOST_MODULE}`"
            ),
            Self::ReservedImport { name } => write!(
                f,
                "`{HOST_MODULE}::{name}` is reserved for the instrumentation pass and may not be \
                 imported directly"
            ),
            Self::ReservedExport { name } => write!(
                f,
                "`{name}` is reserved for the instrumentation pass and may not be exported by a \
                 submitted module"
            ),
            Self::UnknownHostFunction { name } => {
                write!(
                    f,
                    "`{HOST_MODULE}::{name}` is not part of the host interface"
                )
            }
            Self::HostSignatureMismatch { name, declared } => write!(
                f,
                "`{HOST_MODULE}::{name}` was imported as {declared}, which is not its signature"
            ),
            Self::NonFunctionHostImport { name } => write!(
                f,
                "`{HOST_MODULE}::{name}` was imported as something other than a function"
            ),
            Self::MissingExport { name } => {
                write!(f, "module does not export `{name}`")
            }
            Self::ReferenceValue { at } => write!(
                f,
                "a reference type appears in {at}; references have no host-independent \
                 representation, so no state commitment can cover one"
            ),
            Self::MultipleTables => write!(
                f,
                "module declares more than one table; the interpreter resolves `call_indirect` \
                 against a single table and ignores the instruction's table index"
            ),
            Self::NonFunctionTable => write!(
                f,
                "a table holds something other than functions; the table is the one place a \
                 reference may live, and only because `call_indirect` selects from it"
            ),
            Self::ReferenceInstruction { operator } => write!(
                f,
                "`{operator}` reads, writes or creates a reference, which cannot enter a state \
                 commitment; the table is fixed once its element segments are applied"
            ),
        }
    }
}

impl std::error::Error for Rejection {}

/// What the rest of the pipeline needs to know about an accepted module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleFacts {
    /// Initial memory size in 64 KiB pages.
    pub memory_pages_min: u32,
    /// Declared maximum memory size in 64 KiB pages. Sizes the memory commitment.
    pub memory_pages_max: u32,
    /// Whether the module reads its work unit input.
    pub imports_input: bool,
    /// Whether the module writes a result. A module that never calls `output` can still be
    /// valid — it is simply useless — so this is reported rather than enforced.
    pub imports_output: bool,
}

/// The signature each host function must be imported with.
fn host_signature(name: &str) -> Option<(&'static [ValType], &'static [ValType])> {
    match name {
        HOST_INPUT => Some((&[ValType::I32, ValType::I32], &[ValType::I32])),
        HOST_OUTPUT => Some((&[ValType::I32, ValType::I32], &[])),
        _ => None,
    }
}

/// Render a signature the way the error messages do.
fn render_signature(params: &[ValType], results: &[ValType]) -> String {
    fn list(types: &[ValType]) -> String {
        types
            .iter()
            .map(|t| format!("{t:?}").to_lowercase())
            .collect::<Vec<_>>()
            .join(", ")
    }
    format!("({}) -> ({})", list(params), list(results))
}

/// Check a submitted workload module.
///
/// This is the gate a workload passes on its way into the network, before instrumentation.
/// It runs `wasmparser`'s own validator restricted to [`admitted_features`], then applies
/// Cairn's structural rules.
///
/// # Known gap
///
/// The *signature* of [`ENTRY_POINT`] is not checked here, only its presence. A module
/// exporting `cairn_run` with the wrong signature is accepted at registration and fails
/// deterministically at instantiation on the worker instead. That is safe — the failure is
/// identical on every machine — but it wastes a dispatch, so it is worth closing once the
/// engine lands and the expected signature is fixed.
///
/// # Errors
///
/// Returns the first [`Rejection`] found. Validation is not exhaustive — a module with several
/// problems reports one of them — because the audience is a workload author iterating, not a
/// report.
pub fn validate_submitted(bytes: &[u8], limits: Limits) -> Result<ModuleFacts, Rejection> {
    if bytes.len() > limits.max_module_bytes {
        return Err(Rejection::TooLarge {
            bytes: bytes.len(),
            limit: limits.max_module_bytes,
        });
    }

    // Spec-level validation under the restricted feature set. Anything using a forbidden
    // proposal fails here, with a message naming the proposal.
    Validator::new_with_features(admitted_features())
        .validate_all(bytes)
        .map_err(|e| Rejection::Invalid {
            detail: e.to_string(),
        })?;

    inspect(bytes, limits)
}

/// The memory ceiling a module declares, in 64 KiB pages. Reads one section and checks nothing.
///
/// A volunteer needs this number *before* it runs anything, because it is the only honest way to
/// answer "how many of these can this machine run at once" — see `worker-native/src/capacity.rs`.
/// That question cannot be answered through [`validate_submitted`]: the bytes a volunteer is sent
/// are already **canonical**, validated and instrumented once at registration, and re-running the
/// whole gate on every worker would pay for the entire admission check to learn a single integer.
///
/// So this is deliberately not a check. A module with no memory section, or one that declares no
/// maximum, returns `None`, and the caller's business is then to be careful rather than to
/// reject — [`validate_submitted`] already refused both of those at registration, and a volunteer
/// that started second-guessing the coordinator's admission decisions would be a second, weaker
/// implementation of the gate.
///
/// Instrumentation adds functions, globals and calls; it never touches the memory section. So a
/// submitted module and the canonical module made from it declare the same ceiling, which is what
/// makes this usable on the bytes a worker actually holds.
#[must_use]
pub fn declared_memory_pages(bytes: &[u8]) -> Option<u32> {
    for payload in Parser::new(0).parse_all(bytes) {
        let Ok(Payload::MemorySection(reader)) = payload else {
            continue;
        };
        // The first memory, because Cairn admits exactly one. Taking the first rather than
        // insisting on it keeps this a reader: a hypothetical second memory is the gate's
        // problem, not this function's.
        let memory = reader.into_iter().next()?.ok()?;
        return u32::try_from(memory.maximum?).ok();
    }
    None
}

/// Walk the module applying Cairn's structural rules. Assumes spec validity.
fn inspect(bytes: &[u8], limits: Limits) -> Result<ModuleFacts, Rejection> {
    // Function types, in type-section order. Needed to check host import signatures.
    let mut func_types: Vec<(Vec<ValType>, Vec<ValType>)> = Vec::new();

    let mut memory: Option<MemoryType> = None;
    let mut imports_input = false;
    let mut imports_output = false;
    let mut exports_entry = false;
    let mut exports_memory = false;

    for payload in Parser::new(0).parse_all(bytes) {
        let payload = payload.map_err(|e| Rejection::Invalid {
            detail: e.to_string(),
        })?;

        match payload {
            Payload::TypeSection(reader) => {
                for group in reader {
                    let group = group.map_err(|e| Rejection::Invalid {
                        detail: e.to_string(),
                    })?;
                    for sub in group.into_types() {
                        match sub.composite_type.inner {
                            CompositeInnerType::Func(ft) => {
                                // Every function signature in the module passes through here, so
                                // this is where a reference reaching a parameter or a result is
                                // caught — the two positions a `funcref` would arrive in if a
                                // toolchain started passing function pointers as values rather
                                // than as table indices.
                                let index = func_types.len();
                                if let Some(ty) = ft
                                    .params()
                                    .iter()
                                    .chain(ft.results())
                                    .find(|ty| is_reference(**ty))
                                {
                                    return Err(Rejection::ReferenceValue {
                                        at: format!("function type {index} ({ty:?})"),
                                    });
                                }
                                func_types.push((ft.params().to_vec(), ft.results().to_vec()));
                            }
                            // Array and struct types belong to the GC proposal, which the
                            // feature gate already rejected. Recorded as a placeholder so
                            // that type indices stay aligned if that ever changes.
                            _ => func_types.push((Vec::new(), Vec::new())),
                        }
                    }
                }
            }

            Payload::ImportSection(reader) => {
                // `into_imports` flattens the compact-import encoding into individual
                // entries. Compact imports are outside the admitted feature set, so in
                // practice each entry is already single, but flattening keeps this correct if
                // the allowlist ever grows.
                for import in reader.into_imports() {
                    let import = import.map_err(|e| Rejection::Invalid {
                        detail: e.to_string(),
                    })?;

                    if import.module != HOST_MODULE {
                        return Err(Rejection::ForeignImport {
                            module: import.module.to_owned(),
                            name: import.name.to_owned(),
                        });
                    }

                    if import.name == HOST_CHARGE {
                        return Err(Rejection::ReservedImport {
                            name: import.name.to_owned(),
                        });
                    }

                    let TypeRef::Func(type_index) = import.ty else {
                        if matches!(import.ty, TypeRef::Memory(_)) {
                            return Err(Rejection::ImportedMemory);
                        }
                        return Err(Rejection::NonFunctionHostImport {
                            name: import.name.to_owned(),
                        });
                    };

                    let Some((want_params, want_results)) = host_signature(import.name) else {
                        return Err(Rejection::UnknownHostFunction {
                            name: import.name.to_owned(),
                        });
                    };

                    let Some((params, results)) = func_types.get(type_index as usize) else {
                        return Err(Rejection::Invalid {
                            detail: format!("import refers to undeclared type index {type_index}"),
                        });
                    };

                    if params.as_slice() != want_params || results.as_slice() != want_results {
                        return Err(Rejection::HostSignatureMismatch {
                            name: import.name.to_owned(),
                            declared: render_signature(params, results),
                        });
                    }

                    match import.name {
                        HOST_INPUT => imports_input = true,
                        HOST_OUTPUT => imports_output = true,
                        _ => {}
                    }
                }
            }

            Payload::MemorySection(reader) => {
                for mem in reader {
                    let mem = mem.map_err(|e| Rejection::Invalid {
                        detail: e.to_string(),
                    })?;
                    if memory.is_some() {
                        return Err(Rejection::MultipleMemories);
                    }
                    memory = Some(mem);
                }
            }

            Payload::StartSection { .. } => return Err(Rejection::StartSection),

            // The table is the one place a reference is allowed, and only in the narrow sense
            // that `call_indirect` selects from it. One table, holding functions, and nothing
            // that can change it — see `touches_a_reference`.
            Payload::TableSection(reader) => {
                let mut seen = 0;
                for table in reader {
                    let table = table.map_err(|e| Rejection::Invalid {
                        detail: e.to_string(),
                    })?;
                    seen += 1;
                    if seen > 1 {
                        return Err(Rejection::MultipleTables);
                    }
                    if !table.ty.element_type.is_func_ref() {
                        return Err(Rejection::NonFunctionTable);
                    }
                }
            }

            // A global holds a value between instructions and `StateCommitment` hashes every one
            // of them, so a reference here is the plainest form of the thing that cannot be
            // committed to.
            Payload::GlobalSection(reader) => {
                for (index, global) in reader.into_iter().enumerate() {
                    let global = global.map_err(|e| Rejection::Invalid {
                        detail: e.to_string(),
                    })?;
                    if is_reference(global.ty.content_type) {
                        return Err(Rejection::ReferenceValue {
                            at: format!("global {index}"),
                        });
                    }
                }
            }

            // Locals and instructions. This is the pass that the feature gate used to make
            // unnecessary: with reference types enabled for their encoding, `ref.func` and
            // `table.set` now *parse*, and refusing them is this function's job.
            //
            // Refused here rather than left to trap in the interpreter. Every other engine
            // executes these instructions perfectly well, so a module carrying one would run to
            // completion for a volunteer and trap for the referee — which convicts the volunteer.
            Payload::CodeSectionEntry(body) => {
                let locals = body.get_locals_reader().map_err(|e| Rejection::Invalid {
                    detail: e.to_string(),
                })?;
                for local in locals {
                    let (_, ty) = local.map_err(|e| Rejection::Invalid {
                        detail: e.to_string(),
                    })?;
                    if is_reference(ty) {
                        return Err(Rejection::ReferenceValue {
                            at: format!("a local ({ty:?})"),
                        });
                    }
                }

                let operators = body
                    .get_operators_reader()
                    .map_err(|e| Rejection::Invalid {
                        detail: e.to_string(),
                    })?;
                for op in operators {
                    let op = op.map_err(|e| Rejection::Invalid {
                        detail: e.to_string(),
                    })?;
                    if touches_a_reference(&op) {
                        return Err(Rejection::ReferenceInstruction {
                            operator: format!("{op:?}"),
                        });
                    }
                }
            }

            Payload::ExportSection(reader) => {
                for export in reader {
                    let export = export.map_err(|e| Rejection::Invalid {
                        detail: e.to_string(),
                    })?;
                    match (export.name, export.kind) {
                        (ENTRY_POINT, ExternalKind::Func) => exports_entry = true,
                        (MEMORY_EXPORT, ExternalKind::Memory) => exports_memory = true,
                        // Refused whatever it names. The instrumentation pass appends this
                        // export itself, and two exports of one name is not a valid module —
                        // so a submitted module claiming the name would make instrumentation
                        // fail at registration rather than here, with a worse message.
                        (FUEL_EXPORT, _) => {
                            return Err(Rejection::ReservedExport {
                                name: FUEL_EXPORT.to_owned(),
                            })
                        }
                        _ => {}
                    }
                }
            }

            _ => {}
        }
    }

    let memory = memory.ok_or(Rejection::NoMemory)?;

    if memory.shared {
        return Err(Rejection::SharedMemory);
    }
    if memory.memory64 {
        return Err(Rejection::Memory64);
    }
    if memory.page_size_log2.is_some() {
        return Err(Rejection::CustomPageSize);
    }

    let maximum = memory.maximum.ok_or(Rejection::UnboundedMemory)?;
    if maximum > u64::from(limits.max_memory_pages) {
        return Err(Rejection::MemoryTooLarge {
            pages: maximum,
            limit: limits.max_memory_pages,
        });
    }

    if !exports_entry {
        return Err(Rejection::MissingExport {
            name: ENTRY_POINT.to_owned(),
        });
    }
    if !exports_memory {
        return Err(Rejection::MissingExport {
            name: MEMORY_EXPORT.to_owned(),
        });
    }

    // Both casts are safe: `maximum` was just bounded by `max_memory_pages`, a u32, and the
    // validator guarantees `initial <= maximum`.
    Ok(ModuleFacts {
        memory_pages_min: memory.initial as u32,
        memory_pages_max: maximum as u32,
        imports_input,
        imports_output,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// Assemble WebAssembly text into a binary. Panics on malformed test input, which is a
    /// bug in the test rather than a case under test.
    fn wasm(text: &str) -> Vec<u8> {
        wat::parse_str(text).expect("test module should assemble")
    }

    /// A module that satisfies every structural rule, used as the base for negative cases.
    const VALID: &str = r#"
        (module
          (import "cairn" "input"  (func $input  (param i32 i32) (result i32)))
          (import "cairn" "output" (func $output (param i32 i32)))
          (memory (export "memory") 1 16)
          (func (export "cairn_run")
            (drop (call $input (i32.const 0) (i32.const 0)))
            (call $output (i32.const 0) (i32.const 0))
          )
        )
    "#;

    fn check(text: &str) -> Result<ModuleFacts, Rejection> {
        validate_submitted(&wasm(text), Limits::default())
    }

    #[test]
    fn accepts_a_well_formed_module() {
        let facts = check(VALID).expect("the reference module must validate");
        assert_eq!(
            facts,
            ModuleFacts {
                memory_pages_min: 1,
                memory_pages_max: 16,
                imports_input: true,
                imports_output: true,
            }
        );
    }

    #[test]
    fn the_declared_ceiling_survives_instrumentation() {
        // The claim `declared_memory_pages` is built on, and the reason a volunteer may read it
        // from the canonical bytes it was actually sent rather than from a submission it never
        // sees. If instrumentation ever grew a reason to touch the memory section — a scratch
        // page for metering, say — a volunteer would silently budget for the wrong workload,
        // and this is where that shows up.
        let submitted = wasm(VALID);
        let facts = validate_submitted(&submitted, Limits::default()).expect("must validate");
        assert_eq!(
            declared_memory_pages(&submitted),
            Some(facts.memory_pages_max)
        );

        for config in [
            crate::canon::Config::honest_path(),
            crate::canon::Config::dispute_path(),
        ] {
            let canonical = crate::canon::instrument(&submitted, config).expect("must instrument");
            assert_eq!(
                declared_memory_pages(&canonical),
                Some(facts.memory_pages_max),
                "instrumentation changed the declared memory ceiling"
            );
        }
    }

    #[test]
    fn a_module_with_nothing_to_read_reports_nothing_rather_than_a_guess() {
        // Both of these were refused at registration, so a volunteer only meets them if the
        // coordinator served something it never admitted. `None` is what makes the caller
        // assume the worst the network permits; a default here would hide that.
        assert_eq!(declared_memory_pages(&[]), None);
        assert_eq!(
            declared_memory_pages(&wasm(r#"(module (memory 1) (func (export "cairn_run")))"#)),
            None,
            "a memory with no declared maximum has no ceiling to report"
        );
    }

    #[test]
    fn accepts_a_module_that_imports_nothing() {
        // A workload with its input compiled in is unusual but not invalid.
        let facts = check(
            r#"(module
                 (memory (export "memory") 2 4)
                 (func (export "cairn_run")))"#,
        )
        .unwrap();
        assert!(!facts.imports_input);
        assert!(!facts.imports_output);
        assert_eq!(facts.memory_pages_min, 2);
    }

    #[test]
    fn accepts_floating_point() {
        // The workloads Cairn exists for are almost entirely floating point. NaN divergence
        // is handled by canonicalization in the instrumentation pass, not by banning floats.
        check(
            r#"(module
                 (memory (export "memory") 1 1)
                 (func (export "cairn_run") (result f64)
                   (f64.sqrt (f64.mul (f64.const 1.5) (f64.const 2.0)))))"#,
        )
        .expect("floating point must be admitted");
    }

    #[test]
    fn accepts_bulk_memory_and_sign_extension() {
        // Both are emitted by default by current LLVM. Rejecting them would exclude
        // essentially every real compiler output.
        check(
            r#"(module
                 (memory (export "memory") 1 4)
                 (func (export "cairn_run")
                   (memory.fill (i32.const 0) (i32.const 0) (i32.const 16))
                   (drop (i32.extend8_s (i32.const 1)))))"#,
        )
        .expect("bulk memory and sign extension must be admitted");
    }

    // --- Nondeterminism: rejected by the feature gate -----------------------------------
    //
    // These are caught by wasmparser's validator under `admitted_features`, so they surface
    // as `Invalid` with a message naming the proposal. The assertions check that they are
    // refused rather than which variant is produced, because which layer catches them is an
    // implementation detail that may change.

    #[test]
    fn rejects_threads() {
        let err = check(
            r#"(module
                 (memory (export "memory") 1 1 shared)
                 (func (export "cairn_run")))"#,
        )
        .unwrap_err();
        assert!(matches!(err, Rejection::Invalid { .. }), "got {err:?}");
    }

    #[test]
    fn rejects_atomics() {
        let err = check(
            r#"(module
                 (memory (export "memory") 1 1)
                 (func (export "cairn_run")
                   (drop (i32.atomic.load (i32.const 0)))))"#,
        )
        .unwrap_err();
        assert!(matches!(err, Rejection::Invalid { .. }), "got {err:?}");
    }

    #[test]
    fn rejects_simd() {
        // Deterministic in principle, but not implemented by the interpreter. The allowlist
        // and the interpreter's coverage move together.
        let err = check(
            r#"(module
                 (memory (export "memory") 1 1)
                 (func (export "cairn_run")
                   (drop (v128.const i32x4 1 2 3 4))))"#,
        )
        .unwrap_err();
        assert!(matches!(err, Rejection::Invalid { .. }), "got {err:?}");
    }

    // --- Reference types: the encoding is admitted, the values are not -------------------
    //
    // These used to be one test asserting the whole proposal was refused by the feature gate.
    // That refusal also excluded most compiler output, for a reason that turned out to be
    // about how a zero is spelled — see `admitted_features`. The proposal is now enabled and
    // the rules below are structural, so each of them names the position it protects.

    /// A LEB128 unsigned integer, appended.
    fn leb(out: &mut Vec<u8>, mut value: u32) {
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            out.push(if value == 0 { byte } else { byte | 0x80 });
            if value == 0 {
                return;
            }
        }
    }

    /// A section: its id, its payload's length, its payload.
    fn section(out: &mut Vec<u8>, id: u8, payload: &[u8]) {
        out.push(id);
        leb(out, payload.len() as u32);
        out.extend_from_slice(payload);
    }

    #[test]
    fn accepts_the_call_indirect_every_current_toolchain_emits() {
        // The finding this whole change came from. `rustc` writes `call_indirect`'s table index
        // as a **padded five-byte LEB128**, which the base specification does not permit in that
        // position, so the module was refused with `zero byte expected` — with one table, index
        // zero, and no reference anywhere near a value.
        //
        // Assembled byte by byte rather than from `.wat`, because the text format cannot express
        // the padding: `wat` emits the single-byte form, which is exactly the form that already
        // worked. Splicing the extra bytes into an assembled module does not work either — it
        // invalidates the two length prefixes above it, which is how the first version of this
        // test failed with `malformed section id`.
        let mut types = vec![1u8, 0x60, 0x00, 0x00]; // one type: () -> ()
        let functions = vec![2u8, 0x00, 0x00]; // two functions, both of type 0
        let tables = vec![1u8, 0x70, 0x01, 0x01, 0x01]; // one funcref table, 1..=1
        let memories = vec![1u8, 0x01, 0x01, 0x01]; // one memory, 1..=1
        let mut exports = vec![2u8];
        exports.extend_from_slice(b"\x06memory\x02\x00");
        exports.extend_from_slice(b"\x09cairn_run\x00\x01");
        let elements = vec![1u8, 0x00, 0x41, 0x00, 0x0b, 0x01, 0x00]; // table[0] = func 0

        let target = [0x00u8, 0x0b]; // no locals, end
        let caller = [
            0x00, // no locals
            0x41, 0x00, // i32.const 0
            0x11, 0x00, // call_indirect, type 0
            0x80, 0x80, 0x80, 0x80, 0x00, // table 0, the way a compiler spells it
            0x0b, // end
        ];
        let mut code = vec![2u8];
        for body in [&target[..], &caller[..]] {
            leb(&mut code, body.len() as u32);
            code.extend_from_slice(body);
        }

        let mut module = b"\0asm\x01\0\0\0".to_vec();
        section(&mut module, 1, &types);
        section(&mut module, 3, &functions);
        section(&mut module, 4, &tables);
        section(&mut module, 5, &memories);
        section(&mut module, 7, &exports);
        section(&mut module, 9, &elements);
        section(&mut module, 10, &code);

        validate_submitted(&module, Limits::default())
            .expect("the padded form is what every real workload arrives in");

        // And the single-byte form still works, so this widened the gate rather than moving it.
        types.clear();
        let mut narrow = b"\0asm\x01\0\0\0".to_vec();
        section(&mut narrow, 1, &[1u8, 0x60, 0x00, 0x00]);
        section(&mut narrow, 3, &functions);
        section(&mut narrow, 4, &tables);
        section(&mut narrow, 5, &memories);
        section(&mut narrow, 7, &exports);
        section(&mut narrow, 9, &elements);
        let mut narrow_code = vec![2u8];
        let short_caller = [0x00u8, 0x41, 0x00, 0x11, 0x00, 0x00, 0x0b];
        for body in [&target[..], &short_caller[..]] {
            leb(&mut narrow_code, body.len() as u32);
            narrow_code.extend_from_slice(body);
        }
        section(&mut narrow, 10, &narrow_code);
        validate_submitted(&narrow, Limits::default())
            .expect("the spelling the specification requires must still be admitted");
    }

    #[test]
    fn rejects_a_reference_reaching_a_value_position() {
        // The property the blanket refusal used to buy, kept. A reference has no
        // host-independent representation, so `state::Value` has no case for one and a trace
        // commitment cannot cover one — in a parameter, a result, a local or a global.
        //
        // All `funcref`, because `externref` turns out to need the GC feature in this
        // `wasmparser`, so the feature gate refuses it before this pass sees it. That makes
        // these rules narrower in practice than they read, and they are written to stand on
        // their own anyway — the same reasoning `Rejection`'s own documentation gives for
        // keeping `Memory64` and `CustomPageSize`.
        for source in [
            r#"(module (memory (export "memory") 1 1)
                 (func (export "cairn_run"))
                 (func (param funcref)))"#,
            r#"(module (memory (export "memory") 1 1)
                 (func (export "cairn_run"))
                 (func (result funcref) (ref.null func)))"#,
            r#"(module (memory (export "memory") 1 1)
                 (global (mut funcref) (ref.null func))
                 (func (export "cairn_run")))"#,
            r#"(module (memory (export "memory") 1 1)
                 (func (export "cairn_run") (local funcref)))"#,
        ] {
            let err = check(source).unwrap_err();
            assert!(
                matches!(err, Rejection::ReferenceValue { .. }),
                "got {err:?} for {source}"
            );
        }
    }

    #[test]
    fn rejects_every_instruction_that_could_change_the_table() {
        // `StateCommitment` does not cover the table, and the reason it does not need to is
        // that nothing can change it once its element segments are applied. These instructions
        // are what would make that false, so refusing them is what keeps the commitment honest.
        //
        // Refused at the gate rather than left to trap in the interpreter. Every other engine
        // runs them, so a module carrying one completes for a volunteer and traps for the
        // referee — and the referee convicts the volunteer.
        for body in [
            "(drop (table.get (i32.const 0)))",
            "(table.set (i32.const 0) (ref.null func))",
            "(drop (table.size))",
            "(drop (table.grow (ref.null func) (i32.const 1)))",
            "(table.fill (i32.const 0) (ref.null func) (i32.const 1))",
            "(drop (ref.func $target))",
            "(drop (ref.is_null (ref.null func)))",
        ] {
            // The `elem declare` is what makes `ref.func` legal at all: the specification wants
            // a function to be named in an element segment before code may take a reference to
            // it. Without it that one case is refused as invalid, which would have proved
            // nothing about this pass.
            let source = format!(
                r#"(module
                     (memory (export "memory") 1 1)
                     (table 4 4 funcref)
                     (func $target)
                     (elem declare func $target)
                     (func (export "cairn_run") {body}))"#
            );
            let err = check(&source).unwrap_err();
            assert!(
                matches!(
                    err,
                    Rejection::ReferenceInstruction { .. } | Rejection::ReferenceValue { .. }
                ),
                "got {err:?} for {body}"
            );
        }
    }

    #[test]
    fn rejects_a_second_table() {
        // Multiple tables arrive with the proposal. A second one would be indexed correctly by
        // every engine except Cairn's, whose `call_indirect` resolves against a single table and
        // ignores the instruction's table index — a consensus divergence, not a missing feature.
        //
        // `NonFunctionTable` has no test beside this one because it cannot currently be reached:
        // the only non-function element type is `externref`, and this `wasmparser` puts that
        // behind the GC feature. It is kept for the reason `Rejection` states — the structural
        // pass has to stay correct if the allowlist widens again, and a rule that quietly leaned
        // on the feature gate would fail open at exactly that moment.
        let err = check(
            r#"(module
                 (memory (export "memory") 1 1)
                 (table 1 1 funcref)
                 (table 1 1 funcref)
                 (func (export "cairn_run")))"#,
        )
        .unwrap_err();
        assert!(matches!(err, Rejection::MultipleTables), "got {err:?}");
    }

    #[test]
    fn rejects_memory64_and_custom_page_sizes() {
        // Both would break the assumption that the memory commitment is over 64 KiB pages.
        assert!(check(
            r#"(module
                 (memory (export "memory") i64 1 1)
                 (func (export "cairn_run")))"#,
        )
        .is_err());

        assert!(check(
            r#"(module
                 (memory (export "memory") 1 1 (pagesize 1))
                 (func (export "cairn_run")))"#,
        )
        .is_err());
    }

    // --- Cairn's own structural rules ---------------------------------------------------

    #[test]
    fn rejects_a_module_with_no_memory() {
        assert_eq!(
            check(r#"(module (func (export "cairn_run")))"#).unwrap_err(),
            Rejection::NoMemory
        );
    }

    #[test]
    fn rejects_unbounded_memory() {
        // Without a declared maximum the memory's size depends on the host, and so would the
        // point at which growth fails.
        assert_eq!(
            check(
                r#"(module
                     (memory (export "memory") 1)
                     (func (export "cairn_run")))"#,
            )
            .unwrap_err(),
            Rejection::UnboundedMemory
        );
    }

    #[test]
    fn rejects_oversized_memory() {
        let limit = Limits::default().max_memory_pages;
        let text = format!(
            r#"(module
                 (memory (export "memory") 1 {})
                 (func (export "cairn_run")))"#,
            limit + 1
        );
        assert_eq!(
            validate_submitted(&wasm(&text), Limits::default()).unwrap_err(),
            Rejection::MemoryTooLarge {
                pages: u64::from(limit) + 1,
                limit,
            }
        );
    }

    #[test]
    fn rejects_an_imported_memory() {
        let err = check(
            r#"(module
                 (import "cairn" "memory" (memory 1 1))
                 (func (export "cairn_run")))"#,
        )
        .unwrap_err();
        assert_eq!(err, Rejection::ImportedMemory);
    }

    #[test]
    fn rejects_a_start_section() {
        // A start function would run before the entry point and therefore outside the fuel
        // meter, making part of the execution invisible to the trace.
        assert_eq!(
            check(
                r#"(module
                     (memory (export "memory") 1 1)
                     (func $init)
                     (start $init)
                     (func (export "cairn_run")))"#,
            )
            .unwrap_err(),
            Rejection::StartSection
        );
    }

    #[test]
    fn rejects_imports_from_other_modules() {
        assert_eq!(
            check(
                r#"(module
                     (import "wasi_snapshot_preview1" "clock_time_get"
                       (func (param i32 i64 i32) (result i32)))
                     (memory (export "memory") 1 1)
                     (func (export "cairn_run")))"#,
            )
            .unwrap_err(),
            Rejection::ForeignImport {
                module: "wasi_snapshot_preview1".to_owned(),
                name: "clock_time_get".to_owned(),
            }
        );
    }

    #[test]
    fn rejects_importing_the_metering_hook() {
        // A workload that could call `charge` directly could lie about how much it had
        // executed, which is the one thing the fuel meter exists to prevent.
        assert_eq!(
            check(
                r#"(module
                     (import "cairn" "charge" (func (param i32)))
                     (memory (export "memory") 1 1)
                     (func (export "cairn_run")))"#,
            )
            .unwrap_err(),
            Rejection::ReservedImport {
                name: "charge".to_owned()
            }
        );
    }

    #[test]
    fn rejects_exporting_the_fuel_counter() {
        // The other half of the same rule. A workload that owned this name could publish a
        // number of its own choosing as the count of its own execution — and under the global
        // metering encoding it could write to it as well.
        //
        // Both kinds are refused, because the name is what identifies the counter to whoever
        // ran the module; a function called `cairn_fuel` would be just as misleading.
        assert_eq!(
            check(
                r#"(module
                     (memory (export "memory") 1 1)
                     (global $g (mut i64) (i64.const 0))
                     (export "cairn_fuel" (global $g))
                     (func (export "cairn_run")))"#,
            )
            .unwrap_err(),
            Rejection::ReservedExport {
                name: "cairn_fuel".to_owned()
            }
        );

        assert_eq!(
            check(
                r#"(module
                     (memory (export "memory") 1 1)
                     (func $f (result i64) (i64.const 0))
                     (export "cairn_fuel" (func $f))
                     (func (export "cairn_run")))"#,
            )
            .unwrap_err(),
            Rejection::ReservedExport {
                name: "cairn_fuel".to_owned()
            }
        );
    }

    #[test]
    fn rejects_unknown_host_functions() {
        assert_eq!(
            check(
                r#"(module
                     (import "cairn" "random" (func (result i32)))
                     (memory (export "memory") 1 1)
                     (func (export "cairn_run")))"#,
            )
            .unwrap_err(),
            Rejection::UnknownHostFunction {
                name: "random".to_owned()
            }
        );
    }

    #[test]
    fn rejects_a_host_function_with_the_wrong_signature() {
        let err = check(
            r#"(module
                 (import "cairn" "input" (func (param i32) (result i64)))
                 (memory (export "memory") 1 1)
                 (func (export "cairn_run")))"#,
        )
        .unwrap_err();
        match err {
            Rejection::HostSignatureMismatch { name, declared } => {
                assert_eq!(name, "input");
                assert_eq!(declared, "(i32) -> (i64)");
            }
            other => panic!("expected a signature mismatch, got {other:?}"),
        }
    }

    #[test]
    fn rejects_a_missing_entry_point() {
        assert_eq!(
            check(
                r#"(module
                     (memory (export "memory") 1 1)
                     (func (export "not_the_entry_point")))"#,
            )
            .unwrap_err(),
            Rejection::MissingExport {
                name: ENTRY_POINT.to_owned()
            }
        );
    }

    #[test]
    fn rejects_an_unexported_memory() {
        // The host reads results out of linear memory, so it must be reachable.
        assert_eq!(
            check(
                r#"(module
                     (memory 1 1)
                     (func (export "cairn_run")))"#,
            )
            .unwrap_err(),
            Rejection::MissingExport {
                name: MEMORY_EXPORT.to_owned()
            }
        );
    }

    #[test]
    fn rejects_an_entry_point_that_is_not_a_function() {
        // Exporting a global under the entry point's name must not satisfy the requirement.
        assert_eq!(
            check(
                r#"(module
                     (memory (export "memory") 1 1)
                     (global (export "cairn_run") i32 (i32.const 0)))"#,
            )
            .unwrap_err(),
            Rejection::MissingExport {
                name: ENTRY_POINT.to_owned()
            }
        );
    }

    #[test]
    fn rejects_an_oversized_module() {
        let bytes = wasm(VALID);
        let limits = Limits {
            max_module_bytes: bytes.len() - 1,
            ..Limits::default()
        };
        assert_eq!(
            validate_submitted(&bytes, limits).unwrap_err(),
            Rejection::TooLarge {
                bytes: bytes.len(),
                limit: bytes.len() - 1,
            }
        );
    }

    #[test]
    fn rejects_a_non_module() {
        assert!(matches!(
            validate_submitted(b"not wasm at all", Limits::default()).unwrap_err(),
            Rejection::Invalid { .. }
        ));
    }

    #[test]
    fn every_rejection_renders_a_message() {
        // Workload authors read these. An empty or panicking Display would be worse than a
        // vague one.
        let samples = [
            Rejection::TooLarge { bytes: 1, limit: 0 },
            Rejection::Invalid {
                detail: "detail".to_owned(),
            },
            Rejection::NoMemory,
            Rejection::MultipleMemories,
            Rejection::SharedMemory,
            Rejection::Memory64,
            Rejection::CustomPageSize,
            Rejection::UnboundedMemory,
            Rejection::MemoryTooLarge { pages: 1, limit: 0 },
            Rejection::ImportedMemory,
            Rejection::StartSection,
            Rejection::ForeignImport {
                module: "m".to_owned(),
                name: "n".to_owned(),
            },
            Rejection::ReservedImport {
                name: "charge".to_owned(),
            },
            Rejection::UnknownHostFunction {
                name: "n".to_owned(),
            },
            Rejection::HostSignatureMismatch {
                name: "input".to_owned(),
                declared: "() -> ()".to_owned(),
            },
            Rejection::NonFunctionHostImport {
                name: "n".to_owned(),
            },
            Rejection::MissingExport {
                name: "e".to_owned(),
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
