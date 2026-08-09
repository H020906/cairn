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
//! meaningful way to hash one, so a module that can place one on the stack cannot be
//! committed to at all. This is why reference types are rejected, and it is a structural
//! reason rather than a scheduling one.
//!
//! # The allowlist is the interpreter's coverage
//!
//! The set of proposals admitted here is deliberately identical to the set the instrumented
//! interpreter implements. Keeping them equal means Cairn can never accept a module it would
//! later be unable to arbitrate — the failure mode where a dispute arrives and the coordinator
//! discovers it cannot replay the disputed instruction. When the interpreter gains an
//! instruction family, this list grows in the same commit, and not before.
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
/// | reference types, GC, function references | host-opaque values cannot enter a state commitment |
/// | custom page sizes, memory control | the memory commitment assumes 64 KiB pages |
/// | multi-memory | one memory means one page tree |
/// | SIMD, tail call, exceptions, memory64, extended const, wide arithmetic | deterministic, but not yet implemented by the interpreter |
/// | component model | not a core module |
#[must_use]
pub fn admitted_features() -> WasmFeatures {
    WasmFeatures::MUTABLE_GLOBAL
        | WasmFeatures::SIGN_EXTENSION
        | WasmFeatures::SATURATING_FLOAT_TO_INT
        | WasmFeatures::MULTI_VALUE
        | WasmFeatures::BULK_MEMORY
        | WasmFeatures::FLOATS
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

    #[test]
    fn rejects_reference_types() {
        // The structural rejection: an externref cannot be hashed into a state commitment in
        // a host-independent way, so a module that can put one on the stack cannot be
        // committed to at all.
        let err = check(
            r#"(module
                 (memory (export "memory") 1 1)
                 (func (export "cairn_run") (param externref)))"#,
        )
        .unwrap_err();
        assert!(matches!(err, Rejection::Invalid { .. }), "got {err:?}");
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
