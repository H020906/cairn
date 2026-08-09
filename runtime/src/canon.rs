//! Rewrites a submitted module into Cairn-canonical form.
//!
//! This pass runs **once**, when a workload is registered, and its output is what every
//! worker executes. That single fact is the reason the pass exists at all.
//!
//! # Why rewriting, rather than constraining the engine
//!
//! The obvious way to make execution deterministic is to make the interpreter behave: force
//! NaN payloads, count instructions, bound the stack. That works for the slow path, which is
//! our own code. It does not work for the fast path, where the module is handed to the
//! browser's WebAssembly engine to get native speed. We cannot reach inside V8 or
//! SpiderMonkey to normalise a NaN after each floating-point operation, and interposing per
//! instruction is exactly the overhead the fast path exists to avoid.
//!
//! So determinism is baked into the binary instead. Both paths execute the *identical*
//! instrumented module, which leaves the two engines nothing to disagree about. See
//! ADR-0003.
//!
//! # What the pass does
//!
//! 1. **Appends the `cairn.charge` import.** Imported function indices are unchanged;
//!    every *defined* function index shifts by one. Calls, exports and element segments are
//!    remapped accordingly.
//! 2. **Meters fuel.** Each maximal straight-line run of instructions is charged for on
//!    entry, so the running instruction count is exact while the counter is touched once per
//!    run rather than once per instruction.
//! 3. **Canonicalizes NaN** after every floating-point operation whose NaN payload the
//!    specification leaves to the implementation.
//! 4. **Drops custom sections.** The canonical module's hash is part of the work unit's
//!    identity, and custom sections carry compiler-dependent content — two builds of the
//!    same workload should produce the same canonical binary. The cost is the name section,
//!    and with it symbol names in any stack trace.
//!
//! Memory ceilings are *not* applied here: [`crate::validate`] already requires a declared
//! maximum, so the module carries its own bound.

use wasm_encoder::reencode::{Reencode, RoundtripReencoder};
use wasm_encoder::{Instruction, ValType as EncValType};
use wasmparser::{CompositeInnerType, Operator, Payload};

use crate::validate::{HOST_CHARGE, HOST_MODULE};

/// The canonical quiet NaN for `f32`: sign clear, exponent all ones, payload MSB set.
const CANONICAL_NAN_F32: u32 = 0x7fc0_0000;

/// The canonical quiet NaN for `f64`.
const CANONICAL_NAN_F64: u64 = 0x7ff8_0000_0000_0000;

/// Where to force a single NaN bit pattern.
///
/// WebAssembly leaves the payload bits of a computed NaN up to the engine, so two honest
/// workers can disagree about them. Cairn's answer is to overwrite every such NaN with one
/// canonical pattern — the question this enum answers is *how often*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Canonicalization {
    /// Never. Only for measuring what the rest of the pass costs; not a shippable setting.
    Never,

    /// After every operation whose NaN payload the specification leaves open.
    ///
    /// Required whenever machine state itself is committed to, which means the **dispute
    /// path**. A state commitment hashes the operand stack and every frame's locals, so a
    /// non-canonical NaN sitting in either one makes two honest workers' commitments differ
    /// even though their results agree.
    #[default]
    Everywhere,

    /// Only where a NaN's payload could become observable — see [`escape_site`].
    ///
    /// Sound on the **honest path**, where nothing but the result is ever compared. A NaN with
    /// an engine-specific payload is still a NaN, and no arithmetic, comparison or branch can
    /// turn that difference into a difference in the answer. Only a handful of operations can,
    /// and this canonicalizes immediately before each of them.
    ///
    /// This is the cheaper setting by a wide margin: the float benchmark executes 2.30× its
    /// bare instruction count under `Everywhere` and essentially its bare count under this.
    /// See [ADR-0006](../../docs/adr/0006-canonicalize-nans-at-escapes-on-the-honest-path.md).
    AtEscapes,
}

/// Which parts of the pass to apply.
///
/// Separable so the benchmark suite can measure what each costs — ADR-0001's efficiency claim
/// depended on that overhead being small, and a claim nobody can measure is a claim nobody
/// should believe. It was not small.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// Insert fuel accounting. Without it there is no trace coordinate system.
    ///
    /// Needed only on the dispute path, since [ADR-0005](../../docs/adr/0005-the-fast-path-cannot-snapshot.md)
    /// moved trace production there.
    pub meter_fuel: bool,
    /// How aggressively to canonicalize NaNs.
    pub canonicalize: Canonicalization,
}

/// Full instrumentation. The safe thing to get by accident.
impl Default for Config {
    fn default() -> Self {
        Self::dispute_path()
    }
}

impl Config {
    /// What a volunteer runs: determinism only, and only where determinism can be observed.
    #[must_use]
    pub const fn honest_path() -> Self {
        Self {
            meter_fuel: false,
            canonicalize: Canonicalization::AtEscapes,
        }
    }

    /// What both parties re-run when a result is disputed: everything.
    #[must_use]
    pub const fn dispute_path() -> Self {
        Self {
            meter_fuel: true,
            canonicalize: Canonicalization::Everywhere,
        }
    }
}

/// Why instrumentation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The module could not be parsed. Run [`crate::validate::validate_submitted`] first.
    Parse {
        /// The underlying parser message.
        detail: String,
    },
    /// The module has no type section, so it declares no functions.
    NoTypeSection,
    /// A function body appeared with no corresponding entry in the function section.
    BodyWithoutSignature {
        /// Position of the offending body in the code section.
        index: usize,
    },
    /// A function's declared type index is not a function type.
    NotAFunctionType {
        /// The offending type index.
        type_index: u32,
    },
    /// The module declares more locals or parameters than the local index space allows.
    LocalIndexOverflow,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Parse { detail } => write!(f, "could not parse module: {detail}"),
            Self::NoTypeSection => write!(f, "module has no type section"),
            Self::BodyWithoutSignature { index } => {
                write!(
                    f,
                    "function body {index} has no entry in the function section"
                )
            }
            Self::NotAFunctionType { type_index } => {
                write!(f, "type index {type_index} is not a function type")
            }
            Self::LocalIndexOverflow => {
                write!(f, "function declares too many locals to instrument")
            }
        }
    }
}

impl std::error::Error for Error {}

/// Rewrite `module` into Cairn-canonical form.
///
/// The input must already have passed [`crate::validate::validate_submitted`]; this pass
/// assumes a valid module using only admitted features and does not re-check that.
///
/// # Errors
///
/// Returns [`Error`] if the module cannot be parsed or is structurally impossible to
/// instrument.
pub fn instrument(module: &[u8], config: Config) -> Result<Vec<u8>, Error> {
    let facts = survey(module)?;

    let mut pass = Instrumenter {
        config,
        imported_funcs: facts.imported_funcs,
        charge_type: facts.type_count,
        charge_func: facts.imported_funcs,
        type_params: facts.type_params,
        function_types: facts.function_types,
        global_widths: facts.global_widths,
        bodies_seen: 0,
        had_import_section: facts.had_import_section,
        emitted_import_section: false,
    };

    let mut out = wasm_encoder::Module::new();
    pass.parse_core_module(&mut out, wasmparser::Parser::new(0), module)
        .map_err(|e| Error::Parse {
            detail: format!("{e:?}"),
        })?;

    Ok(out.finish())
}

/// Facts gathered in a first pass, before rewriting begins.
///
/// A separate survey is needed because the code section is rewritten *while* streaming, and
/// by then some of this (the parameter count of the function being rewritten) has to already
/// be known.
struct Survey {
    imported_funcs: u32,
    type_count: u32,
    /// Parameter count for each type index, or `None` if that index is not a function type.
    type_params: Vec<Option<u32>>,
    /// Type index of each defined function, in function-section order.
    function_types: Vec<u32>,
    /// Float width of each global by index, imported ones first. `None` for integer globals.
    ///
    /// Needed because `global.set` is an escape site only when the global holds a float, and
    /// the canonicalization sequence has to know which width to emit.
    global_widths: Vec<Option<FloatWidth>>,
    had_import_section: bool,
}

/// The float width of a value type, or `None` if it is not a float.
const fn float_width(ty: wasmparser::ValType) -> Option<FloatWidth> {
    match ty {
        wasmparser::ValType::F32 => Some(FloatWidth::F32),
        wasmparser::ValType::F64 => Some(FloatWidth::F64),
        _ => None,
    }
}

fn survey(module: &[u8]) -> Result<Survey, Error> {
    let mut type_params = Vec::new();
    let mut function_types = Vec::new();
    let mut global_widths = Vec::new();
    let mut imported_funcs = 0u32;
    let mut had_type_section = false;
    let mut had_import_section = false;

    for payload in wasmparser::Parser::new(0).parse_all(module) {
        let payload = payload.map_err(|e| Error::Parse {
            detail: e.to_string(),
        })?;
        match payload {
            Payload::TypeSection(reader) => {
                had_type_section = true;
                for group in reader {
                    let group = group.map_err(|e| Error::Parse {
                        detail: e.to_string(),
                    })?;
                    for sub in group.into_types() {
                        type_params.push(match sub.composite_type.inner {
                            CompositeInnerType::Func(ft) => {
                                Some(u32::try_from(ft.params().len()).unwrap_or(u32::MAX))
                            }
                            _ => None,
                        });
                    }
                }
            }
            Payload::ImportSection(reader) => {
                had_import_section = true;
                for import in reader.into_imports() {
                    let import = import.map_err(|e| Error::Parse {
                        detail: e.to_string(),
                    })?;
                    match import.ty {
                        wasmparser::TypeRef::Func(_) => imported_funcs += 1,
                        // Imported globals occupy the low indices, so they must be recorded in
                        // the same pass to keep `global_widths` aligned with global indices.
                        wasmparser::TypeRef::Global(ty) => {
                            global_widths.push(float_width(ty.content_type));
                        }
                        _ => {}
                    }
                }
            }
            Payload::GlobalSection(reader) => {
                for global in reader {
                    let global = global.map_err(|e| Error::Parse {
                        detail: e.to_string(),
                    })?;
                    global_widths.push(float_width(global.ty.content_type));
                }
            }
            Payload::FunctionSection(reader) => {
                for ty in reader {
                    function_types.push(ty.map_err(|e| Error::Parse {
                        detail: e.to_string(),
                    })?);
                }
            }
            _ => {}
        }
    }

    if !had_type_section {
        return Err(Error::NoTypeSection);
    }

    let type_count = u32::try_from(type_params.len()).unwrap_or(u32::MAX);

    Ok(Survey {
        imported_funcs,
        type_count,
        type_params,
        function_types,
        global_widths,
        had_import_section,
    })
}

struct Instrumenter {
    config: Config,
    /// Number of imported functions in the *original* module.
    imported_funcs: u32,
    /// Type index assigned to `charge`, appended after the original types.
    charge_type: u32,
    /// Function index of `charge` in the rewritten module.
    charge_func: u32,
    type_params: Vec<Option<u32>>,
    function_types: Vec<u32>,
    /// Float width of each global by index; see [`Survey::global_widths`].
    global_widths: Vec<Option<FloatWidth>>,
    bodies_seen: usize,
    had_import_section: bool,
    emitted_import_section: bool,
}

impl Instrumenter {
    /// Local index of the `f32` scratch slot for a function with the given locals.
    ///
    /// Locals are indexed parameters-first, so the scratch slots sit immediately after
    /// everything the function declared.
    fn scratch_base(params: u32, declared_locals: u32) -> Result<u32, Error> {
        params
            .checked_add(declared_locals)
            .ok_or(Error::LocalIndexOverflow)
    }

    /// Canonicalize the top of the stack *before* this operator?
    ///
    /// Escape sites, in **both** modes — and the fact that `Everywhere` does this too is not
    /// redundancy, it is what makes the two modes agree.
    ///
    /// A program can build its own NaN with a chosen payload (`f64.reinterpret_i64` of a
    /// constant, or a load of bytes it stored) without ever executing a NaN-*producing*
    /// operation. `Everywhere` leaves such a value alone, correctly: it is the program's own
    /// deterministic data, identical on every engine. But if `AtEscapes` canonicalized it on
    /// the way out while `Everywhere` did not, the two modules would compute different answers
    /// for that program — and observational equivalence between them is what
    /// [ADR-0005](../../docs/adr/0005-the-fast-path-cannot-snapshot.md) rests on. Canonicalizing
    /// at escapes in both modes makes every value that becomes observable canonical under
    /// either, whatever it was upstream.
    fn canonicalization_before(&self, op: &Operator<'_>) -> Option<FloatWidth> {
        match self.config.canonicalize {
            Canonicalization::Never => None,
            Canonicalization::Everywhere | Canonicalization::AtEscapes => {
                escape_site(op, &self.global_widths)
            }
        }
    }

    /// Canonicalize the result *after* this operator?
    ///
    /// Only under `Everywhere`, and only for operators that can mint an engine-chosen NaN.
    /// This is the expensive half — it fires on ordinary arithmetic — and it exists to keep
    /// intermediate values canonical so that a *state commitment* over the operand stack and
    /// locals agrees between engines. On the honest path nothing commits to those, so
    /// `AtEscapes` skips it entirely.
    fn canonicalization_after(&self, op: &Operator<'_>) -> Option<FloatWidth> {
        match self.config.canonicalize {
            Canonicalization::Everywhere => nan_producing(op),
            Canonicalization::Never | Canonicalization::AtEscapes => None,
        }
    }

    /// Emit the import section entry for `charge`.
    fn push_charge_import(&mut self, imports: &mut wasm_encoder::ImportSection) {
        imports.import(
            HOST_MODULE,
            HOST_CHARGE,
            wasm_encoder::EntityType::Function(self.charge_type),
        );
        self.emitted_import_section = true;
    }
}

/// Does this operator end the current straight-line run?
///
/// A run is charged for on entry, so it must not contain anything that could skip the
/// instructions after it. Control transfers and block boundaries both qualify. Calls do not:
/// a call returns to the following instruction, and if it traps the charge has already been
/// paid, which is the conservative direction.
///
/// Over-segmenting is harmless — it costs an extra `charge` call — while under-segmenting
/// would charge for instructions that never ran. Everything structural is treated as a
/// boundary for that reason.
fn ends_run(op: &Operator<'_>) -> bool {
    matches!(
        op,
        Operator::Block { .. }
            | Operator::Loop { .. }
            | Operator::If { .. }
            | Operator::Else
            | Operator::End
            | Operator::Br { .. }
            | Operator::BrIf { .. }
            | Operator::BrTable { .. }
            | Operator::Return
            | Operator::Unreachable
    )
}

/// Width of a floating-point result whose NaN payload the specification leaves open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FloatWidth {
    F32,
    F64,
}

/// Does this operator produce an `f32`/`f64` whose NaN bits are implementation-defined?
///
/// Included: arithmetic and rounding, which can create a NaN from non-NaN operands or
/// choose among several input NaN payloads.
///
/// Deliberately excluded, because the specification fixes their bits exactly: `abs`, `neg`
/// and `copysign` (sign-bit manipulation), `reinterpret` (a bit cast), loads and stores, and
/// conversions from integers, which cannot produce a NaN at all. Comparisons and truncations
/// yield integers. A NaN the program constructed itself — by storing bytes and loading them
/// back — is fully deterministic and is left alone.
fn nan_producing(op: &Operator<'_>) -> Option<FloatWidth> {
    use FloatWidth::{F32, F64};
    Some(match op {
        Operator::F32Add
        | Operator::F32Sub
        | Operator::F32Mul
        | Operator::F32Div
        | Operator::F32Min
        | Operator::F32Max
        | Operator::F32Sqrt
        | Operator::F32Ceil
        | Operator::F32Floor
        | Operator::F32Trunc
        | Operator::F32Nearest
        | Operator::F32DemoteF64 => F32,

        Operator::F64Add
        | Operator::F64Sub
        | Operator::F64Mul
        | Operator::F64Div
        | Operator::F64Min
        | Operator::F64Max
        | Operator::F64Sqrt
        | Operator::F64Ceil
        | Operator::F64Floor
        | Operator::F64Trunc
        | Operator::F64Nearest
        | Operator::F64PromoteF32 => F64,

        _ => return None,
    })
}

/// Does this operator let a NaN's payload bits change something other than a NaN?
///
/// Returns the width of the value **on top of the stack** that must be canonical before the
/// operator runs. This is the whole soundness argument for
/// [`Canonicalization::AtEscapes`](Canonicalization::AtEscapes), so it is worth stating why
/// the list is this short and why everything else is safe to leave alone.
///
/// A NaN with an engine-specific payload is still a NaN. Arithmetic on it yields a NaN.
/// Comparisons against it are false whatever the payload. `br_if` on such a comparison takes
/// the same branch on every engine. Truncation traps on any NaN, and the saturating form
/// yields zero for any NaN. `abs`, `neg`, `min` and `max` yield a NaN. **None of those can
/// turn a payload difference into an answer difference**, so none of them need the value
/// canonical, which is what makes this mode worth having.
///
/// Four things can:
///
/// - **Stores.** The payload lands in linear memory as bytes, and memory is what a workload
///   reports through `cairn.output`.
/// - **`global.set` on a float global.** Same reason: the bits are kept.
/// - **`reinterpret`.** Its entire purpose is to hand the payload over as an integer.
/// - **`copysign`.** The *sign* of a computed NaN is as unspecified as its payload, and
///   `copysign` reads that sign and applies it to an ordinary number. `copysign(1.0, nan)`
///   could be `+1.0` on one engine and `-1.0` on another. Canonicalizing the top operand fixes
///   the sign bit to zero, which fixes the result. Note this is the *only* entry that is about
///   the sign rather than the payload, and it is the one most easily missed.
///
/// Calls are deliberately absent. A float argument to a defined function becomes a callee
/// local, and locals are not observable on the honest path; if the callee stores it, the store
/// is the escape. Cairn's host interface takes no floats at all.
fn escape_site(op: &Operator<'_>, global_widths: &[Option<FloatWidth>]) -> Option<FloatWidth> {
    use FloatWidth::{F32, F64};
    Some(match op {
        Operator::F32Store { .. } | Operator::I32ReinterpretF32 | Operator::F32Copysign => F32,
        Operator::F64Store { .. } | Operator::I64ReinterpretF64 | Operator::F64Copysign => F64,
        Operator::GlobalSet { global_index } => {
            return *global_widths.get(*global_index as usize)?;
        }
        _ => return None,
    })
}

/// Append the canonicalization sequence for a value already on the stack.
///
/// ```text
/// local.tee $scratch      ; keep a copy, value stays on the stack
/// local.get $scratch
/// f32.ne                  ; NaN is the only value unequal to itself
/// if (result f32)
///   f32.const <canonical>
/// else
///   local.get $scratch
/// end
/// ```
fn emit_canonicalization(func: &mut wasm_encoder::Function, width: FloatWidth, scratch: u32) {
    match width {
        FloatWidth::F32 => {
            func.instruction(&Instruction::LocalTee(scratch));
            func.instruction(&Instruction::LocalGet(scratch));
            func.instruction(&Instruction::F32Ne);
            func.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
                EncValType::F32,
            )));
            func.instruction(&Instruction::F32Const(
                f32::from_bits(CANONICAL_NAN_F32).into(),
            ));
            func.instruction(&Instruction::Else);
            func.instruction(&Instruction::LocalGet(scratch));
            func.instruction(&Instruction::End);
        }
        FloatWidth::F64 => {
            func.instruction(&Instruction::LocalTee(scratch));
            func.instruction(&Instruction::LocalGet(scratch));
            func.instruction(&Instruction::F64Ne);
            func.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
                EncValType::F64,
            )));
            func.instruction(&Instruction::F64Const(
                f64::from_bits(CANONICAL_NAN_F64).into(),
            ));
            func.instruction(&Instruction::Else);
            func.instruction(&Instruction::LocalGet(scratch));
            func.instruction(&Instruction::End);
        }
    }
}

impl Reencode for Instrumenter {
    type Error = Error;

    /// Imported functions keep their index; defined functions shift by one to make room for
    /// the appended `charge` import.
    ///
    /// Overriding this one method is what remaps calls, exports and element segments — the
    /// reencoder routes every function reference through here.
    fn function_index(
        &mut self,
        func: u32,
    ) -> Result<u32, wasm_encoder::reencode::Error<Self::Error>> {
        Ok(if func < self.imported_funcs {
            func
        } else {
            func + 1
        })
    }

    /// Copy the original types, then append `charge`'s.
    fn parse_type_section(
        &mut self,
        types: &mut wasm_encoder::TypeSection,
        section: wasmparser::TypeSectionReader<'_>,
    ) -> Result<(), wasm_encoder::reencode::Error<Self::Error>> {
        wasm_encoder::reencode::utils::parse_type_section(self, types, section)?;
        types.ty().function([EncValType::I32], []);
        Ok(())
    }

    /// Copy the original imports, then append `cairn.charge`.
    fn parse_import_section(
        &mut self,
        imports: &mut wasm_encoder::ImportSection,
        section: wasmparser::ImportSectionReader<'_>,
    ) -> Result<(), wasm_encoder::reencode::Error<Self::Error>> {
        wasm_encoder::reencode::utils::parse_import_section(self, imports, section)?;
        self.push_charge_import(imports);
        Ok(())
    }

    /// A module with no imports of its own still needs one for `charge`.
    ///
    /// The hook fires between sections; the import section belongs immediately before the
    /// function section, which every module reaching this pass has, since
    /// [`crate::validate`] requires a defined entry point.
    fn intersperse_section_hook(
        &mut self,
        module: &mut wasm_encoder::Module,
        after: Option<wasm_encoder::SectionId>,
        before: Option<wasm_encoder::SectionId>,
    ) -> Result<(), wasm_encoder::reencode::Error<Self::Error>> {
        if !self.had_import_section
            && !self.emitted_import_section
            && before == Some(wasm_encoder::SectionId::Function)
        {
            let mut imports = wasm_encoder::ImportSection::new();
            self.push_charge_import(&mut imports);
            module.section(&imports);
        }
        wasm_encoder::reencode::utils::intersperse_section_hook(self, module, after, before)
    }

    /// Drop custom sections so that the canonical binary depends only on the module's
    /// semantics, not on which compiler produced it.
    fn parse_custom_section(
        &mut self,
        _module: &mut wasm_encoder::Module,
        _section: wasmparser::CustomSectionReader<'_>,
    ) -> Result<(), wasm_encoder::reencode::Error<Self::Error>> {
        Ok(())
    }

    fn parse_function_body(
        &mut self,
        code: &mut wasm_encoder::CodeSection,
        body: wasmparser::FunctionBody<'_>,
    ) -> Result<(), wasm_encoder::reencode::Error<Self::Error>> {
        let body_index = self.bodies_seen;
        self.bodies_seen += 1;

        let type_index = *self
            .function_types
            .get(body_index)
            .ok_or(Error::BodyWithoutSignature { index: body_index })?;
        let params = self
            .type_params
            .get(type_index as usize)
            .copied()
            .flatten()
            .ok_or(Error::NotAFunctionType { type_index })?;

        // --- locals -----------------------------------------------------------------
        let mut locals: Vec<(u32, EncValType)> = Vec::new();
        let mut declared = 0u32;
        for local in body.get_locals_reader().map_err(parse_err)? {
            let (count, ty) = local.map_err(parse_err)?;
            declared = declared
                .checked_add(count)
                .ok_or(Error::LocalIndexOverflow)?;
            locals.push((count, self.val_type(ty)?));
        }

        // --- operators --------------------------------------------------------------
        // Read before deciding on scratch locals: which ones a function needs depends on what
        // it actually contains.
        let operators: Vec<Operator<'_>> = body
            .get_operators_reader()
            .map_err(parse_err)?
            .into_iter()
            .collect::<Result<_, _>>()
            .map_err(parse_err)?;

        // --- scratch locals, only where they are used -------------------------------
        //
        // Canonicalization needs somewhere to stash a value while testing it against itself.
        // An earlier version gave every function both an `f32` and an `f64` slot whenever the
        // pass was enabled, which is wasteful in a way that only shows up under recursion:
        // benchmarking measured a **2.76× slowdown on a purely integer workload** that gained
        // no canonicalization instructions at all. The cost was entirely two unused slots per
        // frame, paid on every one of ~400,000 calls.
        //
        // So each width is allocated only if some instruction in this function needs it — and
        // which instructions those are depends on the mode, since `AtEscapes` canonicalizes at
        // a different, much smaller set of sites than `Everywhere`.
        let mut needs_f32 = false;
        let mut needs_f64 = false;
        for op in &operators {
            for width in [
                self.canonicalization_before(op),
                self.canonicalization_after(op),
            ] {
                match width {
                    Some(FloatWidth::F32) => needs_f32 = true,
                    Some(FloatWidth::F64) => needs_f64 = true,
                    None => {}
                }
            }
            if needs_f32 && needs_f64 {
                break;
            }
        }

        let scratch_f32 = Instrumenter::scratch_base(params, declared)?;
        // The `f64` slot sits after the `f32` one only when that one exists.
        let scratch_f64 = if needs_f32 {
            scratch_f32
                .checked_add(1)
                .ok_or(Error::LocalIndexOverflow)?
        } else {
            scratch_f32
        };
        if needs_f32 {
            locals.push((1, EncValType::F32));
        }
        if needs_f64 {
            locals.push((1, EncValType::F64));
        }

        // Charge amounts, indexed by operator position. A run begins at position 0 and after
        // every control-flow operator; its length includes the operator that terminates it.
        let mut charge_at = vec![0u32; operators.len()];
        if self.config.meter_fuel {
            let mut run_start = 0usize;
            for (i, op) in operators.iter().enumerate() {
                if ends_run(op) {
                    let len = i - run_start + 1;
                    if let Some(slot) = charge_at.get_mut(run_start) {
                        *slot = u32::try_from(len).unwrap_or(u32::MAX);
                    }
                    run_start = i + 1;
                }
            }
            // A body always ends with `End`, so no run is left open. Should the operator list
            // ever end otherwise, charge the remainder rather than losing it.
            if run_start < operators.len() {
                if let Some(slot) = charge_at.get_mut(run_start) {
                    *slot = u32::try_from(operators.len() - run_start).unwrap_or(u32::MAX);
                }
            }
        }

        // --- emit -------------------------------------------------------------------
        let mut func = wasm_encoder::Function::new(locals);

        for (i, op) in operators.iter().enumerate() {
            if let Some(&amount) = charge_at.get(i) {
                if amount > 0 {
                    func.instruction(&Instruction::I32Const(amount as i32));
                    func.instruction(&Instruction::Call(self.charge_func));
                }
            }

            let scratch = |width| match width {
                FloatWidth::F32 => scratch_f32,
                FloatWidth::F64 => scratch_f64,
            };

            // Before: fix the operand of an operation that could leak a payload. The value is
            // already on top of the stack, which is why this needs no shuffling.
            if let Some(width) = self.canonicalization_before(op) {
                emit_canonicalization(&mut func, width, scratch(width));
            }

            let encoded = self.instruction(op.clone())?;
            func.instruction(&encoded);

            // After: fix the result of an operation that could mint an engine-chosen NaN.
            if let Some(width) = self.canonicalization_after(op) {
                emit_canonicalization(&mut func, width, scratch(width));
            }
        }

        code.function(&func);
        Ok(())
    }
}

/// Wrap a `wasmparser` error as a reencoder error.
fn parse_err(e: wasmparser::BinaryReaderError) -> wasm_encoder::reencode::Error<Error> {
    wasm_encoder::reencode::Error::UserError(Error::Parse {
        detail: e.to_string(),
    })
}

impl From<Error> for wasm_encoder::reencode::Error<Error> {
    fn from(e: Error) -> Self {
        Self::UserError(e)
    }
}

/// Marker so `RoundtripReencoder` stays referenced; the pass deliberately reimplements only
/// the handful of methods above and inherits the rest of its behaviour.
#[allow(dead_code)]
const _: Option<RoundtripReencoder> = None;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::validate::admitted_features;

    fn wasm(text: &str) -> Vec<u8> {
        wat::parse_str(text).expect("test module should assemble")
    }

    fn canon(text: &str) -> Vec<u8> {
        instrument(&wasm(text), Config::default()).expect("instrumentation should succeed")
    }

    /// Validate an instrumented module. The admitted feature set is unchanged by
    /// instrumentation — the pass must not smuggle in anything the gate would refuse.
    fn assert_valid(module: &[u8]) {
        wasmparser::Validator::new_with_features(admitted_features())
            .validate_all(module)
            .expect("instrumented module must still validate");
    }

    /// Operator names of the nth function body, for asserting on injection sites.
    fn operators(module: &[u8], body_index: usize) -> Vec<String> {
        let mut bodies = Vec::new();
        for payload in wasmparser::Parser::new(0).parse_all(module) {
            if let Payload::CodeSectionEntry(body) = payload.unwrap() {
                let ops: Vec<String> = body
                    .get_operators_reader()
                    .unwrap()
                    .into_iter()
                    .map(|op| {
                        let text = format!("{:?}", op.unwrap());
                        text.split([' ', '('])
                            .next()
                            .unwrap_or_default()
                            .trim_end_matches('{')
                            .trim()
                            .to_owned()
                    })
                    .collect();
                bodies.push(ops);
            }
        }
        bodies.into_iter().nth(body_index).expect("body exists")
    }

    /// Imports of a module, as `module::name` strings in index order.
    fn imports(module: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        for payload in wasmparser::Parser::new(0).parse_all(module) {
            if let Payload::ImportSection(reader) = payload.unwrap() {
                for import in reader.into_imports() {
                    let import = import.unwrap();
                    out.push(format!("{}::{}", import.module, import.name));
                }
            }
        }
        out
    }

    const WITH_IMPORTS: &str = r#"
        (module
          (import "cairn" "input"  (func $input  (param i32 i32) (result i32)))
          (import "cairn" "output" (func $output (param i32 i32)))
          (memory (export "memory") 1 4)
          (func $helper (result i32) (i32.const 7))
          (func (export "cairn_run")
            (drop (call $helper))
            (drop (call $input (i32.const 0) (i32.const 0)))
            (call $output (i32.const 0) (i32.const 0)))
        )
    "#;

    #[test]
    fn output_validates_for_a_range_of_shapes() {
        let cases = [
            WITH_IMPORTS,
            // No imports at all — the pass must create an import section.
            r#"(module (memory (export "memory") 1 1) (func (export "cairn_run")))"#,
            // Loops, branches and nested blocks.
            r#"(module
                 (memory (export "memory") 1 1)
                 (func (export "cairn_run") (local $i i32)
                   (block $out
                     (loop $again
                       (br_if $out (i32.ge_u (local.get $i) (i32.const 10)))
                       (local.set $i (i32.add (local.get $i) (i32.const 1)))
                       (br $again)))))"#,
            // if/else with a result, and floating point throughout.
            r#"(module
                 (memory (export "memory") 1 1)
                 (func (export "cairn_run") (param $x f64) (result f64)
                   (if (result f64) (f64.lt (local.get $x) (f64.const 0))
                     (then (f64.sqrt (f64.neg (local.get $x))))
                     (else (f64.div (f64.const 1) (local.get $x))))))"#,
            // Indirect calls through a table.
            r#"(module
                 (memory (export "memory") 1 1)
                 (type $sig (func (result i32)))
                 (table 2 2 funcref)
                 (func $a (type $sig) (i32.const 1))
                 (func $b (type $sig) (i32.const 2))
                 (elem (i32.const 0) $a $b)
                 (func (export "cairn_run")
                   (drop (call_indirect (type $sig) (i32.const 1)))))"#,
            // Unreachable code after a branch.
            r#"(module
                 (memory (export "memory") 1 1)
                 (func (export "cairn_run") (result i32)
                   (return (i32.const 1))
                   (unreachable)))"#,
        ];

        for (i, case) in cases.iter().enumerate() {
            let out = instrument(&wasm(case), Config::default())
                .unwrap_or_else(|e| panic!("case {i} failed to instrument: {e}"));
            assert_valid(&out);
        }
    }

    #[test]
    fn appends_the_charge_import_last() {
        assert_eq!(
            imports(&canon(WITH_IMPORTS)),
            vec!["cairn::input", "cairn::output", "cairn::charge"]
        );
    }

    #[test]
    fn creates_an_import_section_when_there_was_none() {
        let out = canon(r#"(module (memory (export "memory") 1 1) (func (export "cairn_run")))"#);
        assert_eq!(imports(&out), vec!["cairn::charge"]);
        assert_valid(&out);
    }

    #[test]
    fn defined_function_indices_shift_but_imported_ones_do_not() {
        // Original indices: 0 input, 1 output, 2 $helper, 3 cairn_run.
        // Rewritten:        0 input, 1 output, 2 charge, 3 $helper, 4 cairn_run.
        // So the body of cairn_run must now call 3, 0 and 1 — and charge is 2.
        let out = canon(WITH_IMPORTS);
        let mut calls = Vec::new();
        for payload in wasmparser::Parser::new(0).parse_all(&out) {
            if let Payload::CodeSectionEntry(body) = payload.unwrap() {
                for op in body.get_operators_reader().unwrap() {
                    if let Operator::Call { function_index } = op.unwrap() {
                        calls.push(function_index);
                    }
                }
            }
        }
        // charge (2) appears in both bodies; the workload's own calls are 3, 0, 1.
        assert!(
            calls.contains(&3),
            "call to $helper must shift 2 -> 3: {calls:?}"
        );
        assert!(
            calls.contains(&0),
            "call to imported input must stay 0: {calls:?}"
        );
        assert!(
            calls.contains(&1),
            "call to imported output must stay 1: {calls:?}"
        );
        assert!(calls.contains(&2), "charge must be called: {calls:?}");
    }

    #[test]
    fn charges_inside_the_loop_body_not_only_before_it() {
        // If the charge sat before `loop`, an iterating loop would be free after the first
        // pass and an infinite loop would never exhaust its fuel.
        let ops = operators(
            &canon(
                r#"(module
                     (memory (export "memory") 1 1)
                     (func (export "cairn_run") (local $i i32)
                       (loop $again
                         (local.set $i (i32.add (local.get $i) (i32.const 1)))
                         (br_if $again (i32.lt_u (local.get $i) (i32.const 10))))))"#,
            ),
            0,
        );

        let loop_at = ops.iter().position(|o| o == "Loop").expect("loop present");
        let after_loop = ops.get(loop_at + 1..).unwrap_or_default();
        assert_eq!(
            after_loop.first().map(String::as_str),
            Some("I32Const"),
            "a charge must be the first thing inside the loop body: {ops:?}"
        );
        assert_eq!(after_loop.get(1).map(String::as_str), Some("Call"));
    }

    #[test]
    fn canonicalizes_after_arithmetic_but_not_after_sign_operations() {
        // f64.abs and f64.neg are pure sign-bit manipulation and fully specified, so
        // canonicalizing after them would be wasted instructions on every workload.
        let arith = operators(
            &canon(
                r#"(module (memory (export "memory") 1 1)
                     (func (export "cairn_run") (result f64)
                       (f64.mul (f64.const 1) (f64.const 2))))"#,
            ),
            0,
        );
        let mul_at = arith.iter().position(|o| o == "F64Mul").unwrap();
        assert_eq!(
            arith.get(mul_at + 1).map(String::as_str),
            Some("LocalTee"),
            "arithmetic must be followed by canonicalization: {arith:?}"
        );

        let sign = operators(
            &canon(
                r#"(module (memory (export "memory") 1 1)
                     (func (export "cairn_run") (result f64)
                       (f64.abs (f64.const -1))))"#,
            ),
            0,
        );
        let abs_at = sign.iter().position(|o| o == "F64Abs").unwrap();
        assert_ne!(
            sign.get(abs_at + 1).map(String::as_str),
            Some("LocalTee"),
            "f64.abs is bit-exact and must not be canonicalized: {sign:?}"
        );
    }

    #[test]
    fn canonicalization_covers_both_widths() {
        let ops = operators(
            &canon(
                r#"(module (memory (export "memory") 1 1)
                     (func (export "cairn_run") (result f32)
                       (f32.demote_f64 (f64.sqrt (f64.const 2)))))"#,
            ),
            0,
        );
        assert!(ops.contains(&"F64Ne".to_owned()), "f64 path: {ops:?}");
        assert!(ops.contains(&"F32Ne".to_owned()), "f32 path: {ops:?}");
    }

    #[test]
    fn drops_custom_sections() {
        let source = r#"(module
             (@custom "producers" "clang-19")
             (@custom "a-compiler-fingerprint" "build 12345")
             (memory (export "memory") 1 1)
             (func (export "cairn_run")))"#;
        let original = wat::parse_str(source).unwrap();

        let count = |m: &[u8]| {
            wasmparser::Parser::new(0)
                .parse_all(m)
                .filter(|p| matches!(p, Ok(Payload::CustomSection(_))))
                .count()
        };

        // Guard against a vacuous assertion: if the input carried none, dropping them would
        // prove nothing.
        assert_eq!(count(&original), 2, "test input must carry custom sections");

        let out = instrument(&original, Config::default()).unwrap();
        assert_eq!(
            count(&out),
            0,
            "instrumented module must carry no custom sections"
        );
        assert_valid(&out);
    }

    #[test]
    fn compiler_fingerprints_do_not_change_the_canonical_binary() {
        // The reason for dropping them: two builds of the same workload differing only in
        // producer metadata must instrument to identical bytes, because that hash is part of
        // a work unit's identity.
        let body = r#"(memory (export "memory") 1 1) (func (export "cairn_run"))"#;
        let a = wat::parse_str(format!(
            r#"(module (@custom "producers" "clang-19") {body})"#
        ))
        .unwrap();
        let b = wat::parse_str(format!(
            r#"(module (@custom "producers" "clang-20") {body})"#
        ))
        .unwrap();

        assert_ne!(a, b, "inputs must actually differ");
        assert_eq!(
            instrument(&a, Config::default()).unwrap(),
            instrument(&b, Config::default()).unwrap()
        );
    }

    #[test]
    fn config_flags_switch_their_passes_off() {
        let bare = instrument(
            &wasm(WITH_IMPORTS),
            Config {
                meter_fuel: false,
                canonicalize: Canonicalization::Never,
            },
        )
        .unwrap();
        assert_valid(&bare);

        // The charge import is still appended — index remapping depends on it — but nothing
        // calls it.
        assert_eq!(imports(&bare).len(), 3);
        let ops = operators(&bare, 1);
        assert!(!ops.contains(&"LocalTee".to_owned()), "{ops:?}");

        let metered = instrument(
            &wasm(WITH_IMPORTS),
            Config {
                meter_fuel: true,
                canonicalize: Canonicalization::Never,
            },
        )
        .unwrap();
        assert!(metered.len() > bare.len(), "metering must add instructions");
    }

    #[test]
    fn instrumentation_is_deterministic() {
        // The canonical binary's hash is part of a work unit's identity. Two runs of the pass
        // over the same input must be byte-identical, or two coordinators would disagree
        // about what they had dispatched.
        let source = wasm(WITH_IMPORTS);
        let a = instrument(&source, Config::default()).unwrap();
        let b = instrument(&source, Config::default()).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn a_workload_cannot_be_left_unmetered() {
        // Every function body must begin with a charge, including helpers that the entry
        // point calls. An unmetered function would be free compute.
        let out = canon(WITH_IMPORTS);
        for body in 0..2 {
            let ops = operators(&out, body);
            assert_eq!(
                ops.first().map(String::as_str),
                Some("I32Const"),
                "body {body} must open with a charge: {ops:?}"
            );
            assert_eq!(ops.get(1).map(String::as_str), Some("Call"));
        }
    }

    #[test]
    fn rejects_a_module_it_cannot_parse() {
        assert!(matches!(
            instrument(b"not wasm", Config::default()).unwrap_err(),
            Error::Parse { .. }
        ));
    }

    #[test]
    fn every_error_renders_a_message() {
        let samples = [
            Error::Parse {
                detail: "d".to_owned(),
            },
            Error::NoTypeSection,
            Error::BodyWithoutSignature { index: 0 },
            Error::NotAFunctionType { type_index: 0 },
            Error::LocalIndexOverflow,
        ];
        for sample in samples {
            assert!(
                !sample.to_string().is_empty(),
                "empty message for {sample:?}"
            );
        }
    }
}
