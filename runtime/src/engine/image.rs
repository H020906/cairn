//! Decodes an instrumented module into a form the interpreter can execute directly.
//!
//! # Why there is no custom instruction set
//!
//! The obvious design is to translate WebAssembly into a private IR. Cairn does not: the
//! interpreter evaluates [`wasmparser::Operator`] values directly, and this module only adds
//! what an operator sequence alone cannot express — where each branch lands.
//!
//! Two reasons. Translating roughly two hundred opcodes by hand is exactly the kind of
//! mechanical work that quietly produces a transcription error, and here a transcription error
//! is not a bug that shows up in testing but a *consensus divergence* that convicts honest
//! volunteers. And keeping the operator type means that any instruction outside the admitted
//! set arrives at the interpreter as an explicit `Unsupported`, which makes
//! "[`crate::validate`]'s allowlist equals the interpreter's coverage" a property a test can
//! check rather than a comment nobody maintains.
//!
//! # Resolving control flow ahead of time
//!
//! WebAssembly control flow is structured: `br n` targets the `n`-th enclosing label, and the
//! instruction to jump to depends on what kind of label that is — the `end` of a `block`, but
//! the *start* of a `loop`. Scanning for the matching `end` at branch time would make every
//! branch cost a scan of the function. So each `block`, `loop` and `if` is annotated once, at
//! decode time, with the position of its matching `else` and `end`.

use wasmparser::{
    CompositeInnerType, DataKind, ElementItems, ElementKind, Operator, Payload, ValType,
};

use crate::state::Value;
use crate::validate::{ENTRY_POINT, HOST_CHARGE, HOST_INPUT, HOST_MODULE, HOST_OUTPUT};

/// Ceiling on the number of locals in one function, after run-length expansion.
///
/// Locals are declared run-length encoded, so a tiny module can ask for billions of them.
/// The interpreter indexes locals directly, so the declaration is expanded — and therefore
/// has to be bounded, or a malicious workload could exhaust memory at decode time.
pub const MAX_LOCALS_PER_FUNCTION: u32 = 50_000;

/// A host function the module imports.
///
/// The set is closed: [`crate::validate`] rejects anything else, and `charge` is injected by
/// [`crate::canon`] rather than written by a workload author.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostFunction {
    /// `input(ptr, len) -> len`: copy the work unit's input into linear memory.
    Input,
    /// `output(ptr, len)`: record the work unit's result.
    Output,
    /// `charge(instructions)`: the fuel meter and snapshot hook.
    Charge,
}

/// Where a structured control instruction's `else` and `end` are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Control {
    /// Index of the matching `end`.
    pub end: u32,
    /// Index of the matching `else`, for an `if` that has one.
    pub otherwise: Option<u32>,
}

/// A defined function, decoded.
#[derive(Debug, Clone)]
pub struct Function<'a> {
    /// Index into [`Image::types`].
    pub type_index: u32,
    /// One entry per local, run-length declarations already expanded. Parameters are *not*
    /// included; the interpreter concatenates them at call time.
    pub local_types: Vec<ValType>,
    /// The function's operators, including the trailing `end`.
    pub ops: Vec<Operator<'a>>,
    /// Aligned with [`ops`](Self::ops). Populated only at `block`, `loop` and `if`.
    pub control: Vec<Option<Control>>,
}

/// Linear memory as the module declares it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemorySpec {
    /// Pages present at instantiation.
    pub initial_pages: u32,
    /// Declared ceiling. [`crate::validate`] requires this, so growth failure happens at the
    /// same instruction on every machine.
    pub maximum_pages: u32,
}

/// A global, with its initial value already evaluated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GlobalSpec {
    /// Whether the module may write to it.
    pub mutable: bool,
    /// The value the initializer expression evaluates to.
    pub init: Value,
}

/// The function table, if the module has one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableSpec {
    /// Entries present at instantiation.
    pub initial: u32,
    /// Declared ceiling, if any.
    pub maximum: Option<u32>,
}

/// An element segment: function indices destined for the table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElementSegment {
    /// Copied into the table at instantiation.
    Active {
        /// Table offset the segment starts at.
        offset: u32,
        /// Function indices, in order.
        functions: Vec<u32>,
    },
    /// Available to `table.init` but not installed automatically.
    Passive {
        /// Function indices, in order.
        functions: Vec<u32>,
    },
    /// Present only to make function references valid; carries nothing.
    Declared,
}

/// A data segment: bytes destined for linear memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataSegment<'a> {
    /// Copied into memory at instantiation.
    Active {
        /// Byte offset the segment starts at.
        offset: u32,
        /// The bytes.
        bytes: &'a [u8],
    },
    /// Available to `memory.init` but not installed automatically.
    Passive {
        /// The bytes.
        bytes: &'a [u8],
    },
}

/// An instrumented module, decoded and ready to execute.
#[derive(Debug, Clone)]
pub struct Image<'a> {
    /// Function types, in type-section order.
    pub types: Vec<wasmparser::FuncType>,
    /// Type index for every function, imports first, then defined functions.
    pub func_types: Vec<u32>,
    /// Which host function each imported function index refers to.
    pub host_imports: Vec<HostFunction>,
    /// Defined functions, in code-section order. Function index is
    /// `host_imports.len() + position`.
    pub functions: Vec<Function<'a>>,
    /// Linear memory.
    pub memory: MemorySpec,
    /// Globals, in index order.
    pub globals: Vec<GlobalSpec>,
    /// The function table, if declared.
    pub table: Option<TableSpec>,
    /// Element segments, in order.
    pub elements: Vec<ElementSegment>,
    /// Data segments, in order.
    pub data: Vec<DataSegment<'a>>,
    /// Function index of the entry point.
    pub entry: u32,
    /// Function index of `charge`, which the interpreter intercepts rather than calls.
    pub charge: u32,
}

/// Why a module could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageError {
    /// The module could not be parsed.
    Parse {
        /// The underlying parser message.
        detail: String,
    },
    /// An import outside Cairn's host interface. Run [`crate::validate`] first.
    UnsupportedImport {
        /// The module the import named.
        module: String,
        /// The import's name.
        name: String,
    },
    /// The module declares no linear memory, or no maximum for it.
    BadMemory,
    /// The module does not export [`ENTRY_POINT`] as a function.
    NoEntryPoint,
    /// The module never imports `charge`, so it was not instrumented.
    NotInstrumented,
    /// An initializer expression was not a single constant.
    ///
    /// Only constants are reachable here: the `extended-const` proposal is outside the
    /// admitted feature set, and no importable global exists to read from.
    UnsupportedInitializer,
    /// A function declares more locals than [`MAX_LOCALS_PER_FUNCTION`].
    TooManyLocals {
        /// Position of the function in the code section.
        function: usize,
        /// The number of locals it asked for.
        declared: u64,
    },
    /// Structured control instructions did not nest correctly.
    UnbalancedControl {
        /// Position of the function in the code section.
        function: usize,
    },
    /// A function body appeared with no entry in the function section.
    BodyWithoutSignature {
        /// Position of the body in the code section.
        index: usize,
    },
}

impl core::fmt::Display for ImageError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Parse { detail } => write!(f, "could not parse module: {detail}"),
            Self::UnsupportedImport { module, name } => {
                write!(f, "unsupported import `{module}::{name}`")
            }
            Self::BadMemory => write!(f, "module declares no bounded linear memory"),
            Self::NoEntryPoint => write!(f, "module does not export `{ENTRY_POINT}`"),
            Self::NotInstrumented => write!(
                f,
                "module does not import `{HOST_MODULE}::{HOST_CHARGE}`, so it has not been \
                 through the instrumentation pass"
            ),
            Self::UnsupportedInitializer => {
                write!(f, "initializer expression is not a single constant")
            }
            Self::TooManyLocals { function, declared } => write!(
                f,
                "function {function} declares {declared} locals, over the \
                 {MAX_LOCALS_PER_FUNCTION} limit"
            ),
            Self::UnbalancedControl { function } => {
                write!(f, "function {function} has unbalanced control instructions")
            }
            Self::BodyWithoutSignature { index } => {
                write!(
                    f,
                    "function body {index} has no entry in the function section"
                )
            }
        }
    }
}

impl std::error::Error for ImageError {}

fn parse<T>(r: Result<T, wasmparser::BinaryReaderError>) -> Result<T, ImageError> {
    r.map_err(|e| ImageError::Parse {
        detail: e.to_string(),
    })
}

/// Evaluate a constant initializer expression.
///
/// The admitted feature set leaves only the four `*.const` instructions reachable here:
/// `extended-const` is excluded, and `global.get` would need an imported global, which the
/// host interface does not provide.
fn eval_const(expr: &wasmparser::ConstExpr<'_>) -> Result<Value, ImageError> {
    let mut reader = expr.get_operators_reader();
    let value = match parse(reader.read())? {
        Operator::I32Const { value } => Value::I32(value),
        Operator::I64Const { value } => Value::I64(value),
        Operator::F32Const { value } => Value::F32(value.bits()),
        Operator::F64Const { value } => Value::F64(value.bits()),
        _ => return Err(ImageError::UnsupportedInitializer),
    };
    match parse(reader.read())? {
        Operator::End => {}
        _ => return Err(ImageError::UnsupportedInitializer),
    }
    Ok(value)
}

/// A constant initializer that must be a `u32` offset.
fn eval_offset(expr: &wasmparser::ConstExpr<'_>) -> Result<u32, ImageError> {
    match eval_const(expr)? {
        Value::I32(v) => Ok(v as u32),
        _ => Err(ImageError::UnsupportedInitializer),
    }
}

/// Annotate every structured control instruction with the position of its `else` and `end`.
///
/// The function's own trailing `end` closes no explicit block and is left unannotated; the
/// interpreter treats returning past it as returning from the function.
fn resolve_control(
    ops: &[Operator<'_>],
    function: usize,
) -> Result<Vec<Option<Control>>, ImageError> {
    let mut control: Vec<Option<Control>> = vec![None; ops.len()];
    let mut open: Vec<usize> = Vec::new();

    for (i, op) in ops.iter().enumerate() {
        match op {
            Operator::Block { .. } | Operator::Loop { .. } | Operator::If { .. } => {
                if let Some(slot) = control.get_mut(i) {
                    *slot = Some(Control {
                        end: u32::MAX,
                        otherwise: None,
                    });
                }
                open.push(i);
            }
            Operator::Else => {
                let top = *open
                    .last()
                    .ok_or(ImageError::UnbalancedControl { function })?;
                let slot = control
                    .get_mut(top)
                    .and_then(Option::as_mut)
                    .ok_or(ImageError::UnbalancedControl { function })?;
                slot.otherwise = Some(u32::try_from(i).unwrap_or(u32::MAX));
            }
            Operator::End => {
                // The last `end` of a body closes the function itself and has no opener.
                if let Some(top) = open.pop() {
                    let slot = control
                        .get_mut(top)
                        .and_then(Option::as_mut)
                        .ok_or(ImageError::UnbalancedControl { function })?;
                    slot.end = u32::try_from(i).unwrap_or(u32::MAX);
                }
            }
            _ => {}
        }
    }

    if !open.is_empty() {
        return Err(ImageError::UnbalancedControl { function });
    }
    Ok(control)
}

/// Decode an instrumented module.
///
/// The input must be the output of [`crate::canon::instrument`]. Decoding assumes spec
/// validity and only reports the structural problems it cannot proceed past.
///
/// # Errors
///
/// Returns [`ImageError`] if the module cannot be parsed or is not a Cairn-instrumented
/// module.
pub fn decode(module: &[u8]) -> Result<Image<'_>, ImageError> {
    let mut types = Vec::new();
    let mut func_types = Vec::new();
    let mut host_imports = Vec::new();
    let mut functions = Vec::new();
    let mut globals = Vec::new();
    let mut elements = Vec::new();
    let mut data = Vec::new();
    let mut memory: Option<MemorySpec> = None;
    let mut table: Option<TableSpec> = None;
    let mut entry: Option<u32> = None;
    let mut charge: Option<u32> = None;
    let mut defined_types: Vec<u32> = Vec::new();
    let mut bodies = 0usize;

    for payload in wasmparser::Parser::new(0).parse_all(module) {
        match parse(payload)? {
            Payload::TypeSection(reader) => {
                for group in reader {
                    for sub in parse(group)?.into_types() {
                        if let CompositeInnerType::Func(ft) = sub.composite_type.inner {
                            types.push(ft);
                        } else {
                            // GC types are outside the admitted set; push a placeholder so
                            // type indices stay aligned rather than silently shifting.
                            types.push(wasmparser::FuncType::new([], []));
                        }
                    }
                }
            }

            Payload::ImportSection(reader) => {
                for import in reader.into_imports() {
                    let import = parse(import)?;
                    let wasmparser::TypeRef::Func(type_index) = import.ty else {
                        return Err(ImageError::UnsupportedImport {
                            module: import.module.to_owned(),
                            name: import.name.to_owned(),
                        });
                    };
                    let host = match (import.module, import.name) {
                        (HOST_MODULE, HOST_INPUT) => HostFunction::Input,
                        (HOST_MODULE, HOST_OUTPUT) => HostFunction::Output,
                        (HOST_MODULE, HOST_CHARGE) => {
                            charge = Some(u32::try_from(host_imports.len()).unwrap_or(u32::MAX));
                            HostFunction::Charge
                        }
                        _ => {
                            return Err(ImageError::UnsupportedImport {
                                module: import.module.to_owned(),
                                name: import.name.to_owned(),
                            })
                        }
                    };
                    host_imports.push(host);
                    func_types.push(type_index);
                }
            }

            Payload::FunctionSection(reader) => {
                for ty in reader {
                    let ty = parse(ty)?;
                    defined_types.push(ty);
                    func_types.push(ty);
                }
            }

            Payload::MemorySection(reader) => {
                for mem in reader {
                    let mem = parse(mem)?;
                    let maximum = mem.maximum.ok_or(ImageError::BadMemory)?;
                    memory = Some(MemorySpec {
                        initial_pages: u32::try_from(mem.initial).unwrap_or(u32::MAX),
                        maximum_pages: u32::try_from(maximum).unwrap_or(u32::MAX),
                    });
                }
            }

            Payload::TableSection(reader) => {
                for t in reader {
                    let t = parse(t)?;
                    table = Some(TableSpec {
                        initial: u32::try_from(t.ty.initial).unwrap_or(u32::MAX),
                        maximum: t.ty.maximum.map(|m| u32::try_from(m).unwrap_or(u32::MAX)),
                    });
                }
            }

            Payload::GlobalSection(reader) => {
                for g in reader {
                    let g = parse(g)?;
                    globals.push(GlobalSpec {
                        mutable: g.ty.mutable,
                        init: eval_const(&g.init_expr)?,
                    });
                }
            }

            Payload::ExportSection(reader) => {
                for export in reader {
                    let export = parse(export)?;
                    if export.name == ENTRY_POINT && export.kind == wasmparser::ExternalKind::Func {
                        entry = Some(export.index);
                    }
                }
            }

            Payload::ElementSection(reader) => {
                for element in reader {
                    let element = parse(element)?;
                    let mut function_indices = Vec::new();
                    if let ElementItems::Functions(items) = element.items {
                        for item in items {
                            function_indices.push(parse(item)?);
                        }
                    }
                    elements.push(match element.kind {
                        ElementKind::Active { offset_expr, .. } => ElementSegment::Active {
                            offset: eval_offset(&offset_expr)?,
                            functions: function_indices,
                        },
                        ElementKind::Passive => ElementSegment::Passive {
                            functions: function_indices,
                        },
                        ElementKind::Declared => ElementSegment::Declared,
                    });
                }
            }

            Payload::DataSection(reader) => {
                for segment in reader {
                    let segment = parse(segment)?;
                    data.push(match segment.kind {
                        DataKind::Active { offset_expr, .. } => DataSegment::Active {
                            offset: eval_offset(&offset_expr)?,
                            bytes: segment.data,
                        },
                        DataKind::Passive => DataSegment::Passive {
                            bytes: segment.data,
                        },
                    });
                }
            }

            Payload::CodeSectionEntry(body) => {
                let index = bodies;
                bodies += 1;

                let type_index = *defined_types
                    .get(index)
                    .ok_or(ImageError::BodyWithoutSignature { index })?;

                let mut local_types = Vec::new();
                let mut declared: u64 = 0;
                for local in parse(body.get_locals_reader())? {
                    let (count, ty) = parse(local)?;
                    declared += u64::from(count);
                    if declared > u64::from(MAX_LOCALS_PER_FUNCTION) {
                        return Err(ImageError::TooManyLocals {
                            function: index,
                            declared,
                        });
                    }
                    local_types.extend(std::iter::repeat_n(ty, count as usize));
                }

                let ops: Vec<Operator<'_>> = parse(body.get_operators_reader())?
                    .into_iter()
                    .collect::<Result<_, _>>()
                    .map_err(|e| ImageError::Parse {
                        detail: e.to_string(),
                    })?;

                let control = resolve_control(&ops, index)?;

                functions.push(Function {
                    type_index,
                    local_types,
                    ops,
                    control,
                });
            }

            _ => {}
        }
    }

    Ok(Image {
        types,
        func_types,
        host_imports,
        functions,
        memory: memory.ok_or(ImageError::BadMemory)?,
        globals,
        table,
        elements,
        data,
        entry: entry.ok_or(ImageError::NoEntryPoint)?,
        charge: charge.ok_or(ImageError::NotInstrumented)?,
    })
}

impl Image<'_> {
    /// The decoded body of a function index, or `None` if it names an import.
    #[must_use]
    pub fn function(&self, index: u32) -> Option<&Function<'_>> {
        let imports = u32::try_from(self.host_imports.len()).unwrap_or(u32::MAX);
        let defined = index.checked_sub(imports)?;
        self.functions.get(defined as usize)
    }

    /// The host function an index names, or `None` if it names a defined function.
    #[must_use]
    pub fn host_function(&self, index: u32) -> Option<HostFunction> {
        self.host_imports.get(index as usize).copied()
    }

    /// The signature of any function index.
    #[must_use]
    pub fn signature(&self, index: u32) -> Option<&wasmparser::FuncType> {
        let type_index = *self.func_types.get(index as usize)?;
        self.types.get(type_index as usize)
    }
}

#[cfg(test)]
mod tests {
    // Indices in these tests are positions in operator sequences the test itself writes out,
    // so a panic on a bad index is the correct failure and reads better than `.get()`.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use super::*;
    use crate::canon::{self, Canonicalization, Config};

    /// Assemble, then instrument, exactly as a coordinator would.
    fn canonical(text: &str, config: Config) -> Vec<u8> {
        let source = wat::parse_str(text).expect("test module should assemble");
        canon::instrument(&source, config).expect("instrumentation should succeed")
    }

    /// Instrument with both passes off, so operator positions are predictable in tests that
    /// assert on them.
    fn bare(text: &str) -> Vec<u8> {
        canonical(
            text,
            Config {
                meter_fuel: false,
                canonicalize: Canonicalization::Never,
            },
        )
    }

    const SIMPLE: &str = r#"
        (module
          (import "cairn" "input"  (func $input  (param i32 i32) (result i32)))
          (import "cairn" "output" (func $output (param i32 i32)))
          (memory (export "memory") 2 16)
          (global $counter (mut i32) (i32.const 41))
          (data (i32.const 8) "hello")
          (func $helper (result i32) (global.get $counter))
          (func (export "cairn_run")
            (drop (call $helper))
            (call $output (i32.const 0) (i32.const 5)))
        )
    "#;

    #[test]
    fn decodes_the_shape_of_a_module() {
        let module = bare(SIMPLE);
        let image = decode(&module).expect("instrumented module must decode");

        // Imports: input, output, charge — charge appended last by the pass.
        assert_eq!(
            image.host_imports,
            vec![
                HostFunction::Input,
                HostFunction::Output,
                HostFunction::Charge
            ]
        );
        assert_eq!(image.charge, 2);

        // Defined functions shift by one: $helper is 3, cairn_run is 4.
        assert_eq!(image.functions.len(), 2);
        assert_eq!(image.entry, 4);

        assert_eq!(
            image.memory,
            MemorySpec {
                initial_pages: 2,
                maximum_pages: 16
            }
        );
        assert_eq!(
            image.globals,
            vec![GlobalSpec {
                mutable: true,
                init: Value::I32(41)
            }]
        );
        assert_eq!(
            image.data,
            vec![DataSegment::Active {
                offset: 8,
                bytes: b"hello"
            }]
        );
    }

    #[test]
    fn accessors_agree_with_the_index_space() {
        let module = bare(SIMPLE);
        let image = decode(&module).unwrap();

        assert_eq!(image.host_function(0), Some(HostFunction::Input));
        assert_eq!(image.host_function(2), Some(HostFunction::Charge));
        assert!(image.function(0).is_none(), "index 0 is an import");

        assert!(image.function(3).is_some(), "index 3 is $helper");
        assert!(image.function(image.entry).is_some());
        assert!(image.host_function(image.entry).is_none());
        assert!(image.function(99).is_none());

        // charge is (i32) -> ()
        let sig = image
            .signature(image.charge)
            .expect("charge has a signature");
        assert_eq!(sig.params(), &[ValType::I32]);
        assert!(sig.results().is_empty());
    }

    #[test]
    fn resolves_a_block_to_its_end() {
        // ops: Block, Br, End, End
        let module = bare(
            r#"(module (memory (export "memory") 1 1)
                 (func (export "cairn_run") (block (br 0))))"#,
        );
        let image = decode(&module).unwrap();
        let f = image.function(image.entry).unwrap();

        assert!(matches!(f.ops.first(), Some(Operator::Block { .. })));
        assert_eq!(
            f.control[0],
            Some(Control {
                end: 2,
                otherwise: None
            })
        );
        // The function's own trailing `end` closes no explicit block.
        assert_eq!(f.control[3], None);
    }

    #[test]
    fn resolves_an_if_to_both_its_else_and_its_end() {
        // ops: I32Const, If, I32Const, Else, I32Const, End, End
        let module = bare(
            r#"(module (memory (export "memory") 1 1)
                 (func (export "cairn_run") (result i32)
                   (if (result i32) (i32.const 1)
                     (then (i32.const 2))
                     (else (i32.const 3)))))"#,
        );
        let image = decode(&module).unwrap();
        let f = image.function(image.entry).unwrap();

        assert!(matches!(f.ops.get(1), Some(Operator::If { .. })));
        assert_eq!(
            f.control[1],
            Some(Control {
                end: 5,
                otherwise: Some(3)
            })
        );
    }

    #[test]
    fn resolves_nested_control_independently() {
        // A loop inside a block: each must point at its own end, not the outer one.
        let module = bare(
            r#"(module (memory (export "memory") 1 1)
                 (func (export "cairn_run")
                   (block
                     (loop
                       (br 1)))))"#,
        );
        let image = decode(&module).unwrap();
        let f = image.function(image.entry).unwrap();

        // ops: Block, Loop, Br, End(loop), End(block), End(function)
        let block = f.control[0].expect("block annotated");
        let loop_ = f.control[1].expect("loop annotated");
        assert_eq!(loop_.end, 3);
        assert_eq!(block.end, 4);
        assert!(
            block.end > loop_.end,
            "outer block must close after inner loop"
        );
    }

    #[test]
    fn an_if_without_an_else_has_no_otherwise() {
        let module = bare(
            r#"(module (memory (export "memory") 1 1)
                 (func (export "cairn_run")
                   (if (i32.const 1) (then (nop)))))"#,
        );
        let image = decode(&module).unwrap();
        let f = image.function(image.entry).unwrap();
        let if_ = f.control[1].expect("if annotated");
        assert_eq!(if_.otherwise, None);
    }

    #[test]
    fn control_resolution_covers_instructions_the_pass_injected() {
        // NaN canonicalization emits its own `if (result fN) ... else ... end` after every
        // arithmetic operation, so an instrumented module contains structured control the
        // workload author never wrote. The interpreter has to execute those too, which means
        // they must be annotated exactly like the workload's own.
        //
        // The function below has two float operations, so instrumentation adds two `if`s on
        // top of the block, loop and if it declares.
        let source = r#"(module (memory (export "memory") 1 4)
                 (func (export "cairn_run") (param $n i32) (result f64) (local $acc f64)
                   (block $done
                     (loop $again
                       (br_if $done (i32.eqz (local.get $n)))
                       (if (i32.eq (i32.rem_u (local.get $n) (i32.const 2)) (i32.const 0))
                         (then (local.set $acc (f64.add (local.get $acc) (f64.const 1))))
                         (else (local.set $acc (f64.sub (local.get $acc) (f64.const 0.5)))))
                       (local.set $n (i32.sub (local.get $n) (i32.const 1)))
                       (br $again)))
                   (local.get $acc)))"#;

        // The images borrow their module bytes, so both buffers must outlive them.
        let plain = bare(source);
        let full = canonical(source, Config::default());

        let count_control = |module: &[u8]| {
            let image = decode(module).unwrap();
            image
                .function(image.entry)
                .unwrap()
                .control
                .iter()
                .flatten()
                .count()
        };

        let authored = count_control(&plain);
        assert_eq!(authored, 3, "block, loop and if as written");

        let total = count_control(&full);
        assert_eq!(
            total,
            authored + 2,
            "one extra `if` per canonicalized float operation"
        );
    }

    #[test]
    fn every_structured_instruction_gets_an_end() {
        // No annotation may be left at the u32::MAX sentinel, which would mean an opener was
        // never closed and a branch to it would jump into nothing.
        let module = canonical(
            r#"(module (memory (export "memory") 1 4)
                 (func (export "cairn_run") (param $n i32) (result f64) (local $acc f64)
                   (block $done
                     (loop $again
                       (br_if $done (i32.eqz (local.get $n)))
                       (if (i32.eq (i32.rem_u (local.get $n) (i32.const 2)) (i32.const 0))
                         (then (local.set $acc (f64.add (local.get $acc) (f64.const 1))))
                         (else (local.set $acc (f64.sub (local.get $acc) (f64.const 0.5)))))
                       (local.set $n (i32.sub (local.get $n) (i32.const 1)))
                       (br $again)))
                   (local.get $acc)))"#,
            Config::default(),
        );
        let image = decode(&module).unwrap();
        let f = image.function(image.entry).unwrap();

        let annotated: Vec<&Control> = f.control.iter().flatten().collect();
        assert!(!annotated.is_empty(), "the function has structured control");
        for c in annotated {
            assert_ne!(c.end, u32::MAX, "an opener was never closed: {c:?}");
        }
    }

    #[test]
    fn locals_are_expanded_one_entry_each() {
        let module = bare(
            r#"(module (memory (export "memory") 1 1)
                 (func (export "cairn_run") (param i32)
                   (local i64 i64 i64) (local f32)
                   (nop)))"#,
        );
        let image = decode(&module).unwrap();
        let f = image.function(image.entry).unwrap();

        // Parameters are excluded; the interpreter concatenates them at call time.
        assert_eq!(
            f.local_types,
            vec![ValType::I64, ValType::I64, ValType::I64, ValType::F32]
        );
    }

    #[test]
    fn canonicalization_allocates_scratch_locals_only_where_they_are_used() {
        // The pass gives a function a scratch slot per float width it actually canonicalizes,
        // and none otherwise. This is not tidiness: every slot is allocated on every call, so
        // giving them to functions that do not need them costs most exactly where it is least
        // visible — deep recursion. Benchmarking caught a 2.76x slowdown on a purely integer
        // workload that gained no canonicalization instructions at all.
        let scratch_of = |body: &str| {
            let module = canonical(
                &format!(
                    r#"(module (memory (export "memory") 1 1)
                         (func (export "cairn_run") {body}))"#
                ),
                Config::default(),
            );
            let image = decode(&module).unwrap();
            image.function(image.entry).unwrap().local_types.clone()
        };

        assert_eq!(
            scratch_of("(result f64) (f64.sqrt (f64.const 2))"),
            vec![ValType::F64],
            "an f64-only function needs one f64 slot"
        );
        assert_eq!(
            scratch_of("(result f32) (f32.sqrt (f32.const 2))"),
            vec![ValType::F32],
            "an f32-only function needs one f32 slot"
        );
        assert_eq!(
            scratch_of("(result f64) (f64.add (f64.promote_f32 (f32.mul (f32.const 1) (f32.const 2))) (f64.const 1))"),
            vec![ValType::F32, ValType::F64],
            "a function using both widths needs both"
        );
        assert_eq!(
            scratch_of("(result i32) (i32.add (i32.const 1) (i32.const 2))"),
            Vec::<ValType>::new(),
            "an integer-only function gets no scratch at all"
        );
        assert_eq!(
            scratch_of("(result f64) (f64.abs (f64.const -2))"),
            Vec::<ValType>::new(),
            "abs is bit-exact, so it is not canonicalized and needs no scratch"
        );
    }

    #[test]
    fn rejects_a_module_that_was_never_instrumented() {
        // Decoding a raw submission would silently produce an unmetered execution.
        let raw = wat::parse_str(
            r#"(module (memory (export "memory") 1 1) (func (export "cairn_run")))"#,
        )
        .unwrap();
        assert_eq!(decode(&raw).unwrap_err(), ImageError::NotInstrumented);
    }

    #[test]
    fn rejects_a_module_with_no_memory() {
        // Built by hand: validate would refuse this, but decode must not assume it ran.
        let module = wat::parse_str(
            r#"(module (import "cairn" "charge" (func (param i32)))
                 (func (export "cairn_run")))"#,
        )
        .unwrap();
        assert_eq!(decode(&module).unwrap_err(), ImageError::BadMemory);
    }

    #[test]
    fn rejects_a_module_with_no_entry_point() {
        let module = wat::parse_str(
            r#"(module (import "cairn" "charge" (func (param i32)))
                 (memory (export "memory") 1 1)
                 (func (export "something_else")))"#,
        )
        .unwrap();
        assert_eq!(decode(&module).unwrap_err(), ImageError::NoEntryPoint);
    }

    #[test]
    fn rejects_a_foreign_import() {
        let module = wat::parse_str(
            r#"(module
                 (import "cairn" "charge" (func (param i32)))
                 (import "env" "gettimeofday" (func (result i64)))
                 (memory (export "memory") 1 1)
                 (func (export "cairn_run")))"#,
        )
        .unwrap();
        assert_eq!(
            decode(&module).unwrap_err(),
            ImageError::UnsupportedImport {
                module: "env".to_owned(),
                name: "gettimeofday".to_owned()
            }
        );
    }

    /// Build a minimal instrumented module whose entry point declares `locals` locals.
    ///
    /// Written with `wasm-encoder` rather than as text because the point is the *run-length
    /// encoding*: a declaration of a hundred thousand locals is a handful of bytes on the
    /// wire, and writing them out in WebAssembly text would not exercise that.
    fn module_with_local_count(locals: u32) -> Vec<u8> {
        use wasm_encoder as enc;

        let mut types = enc::TypeSection::new();
        types.ty().function([enc::ValType::I32], []); // charge
        types.ty().function([], []); // cairn_run

        let mut imports = enc::ImportSection::new();
        imports.import(HOST_MODULE, HOST_CHARGE, enc::EntityType::Function(0));

        let mut funcs = enc::FunctionSection::new();
        funcs.function(1);

        let mut memories = enc::MemorySection::new();
        memories.memory(enc::MemoryType {
            minimum: 1,
            maximum: Some(1),
            memory64: false,
            shared: false,
            page_size_log2: None,
        });

        let mut exports = enc::ExportSection::new();
        exports.export(ENTRY_POINT, enc::ExportKind::Func, 1);
        exports.export("memory", enc::ExportKind::Memory, 0);

        let mut code = enc::CodeSection::new();
        let mut body = enc::Function::new([(locals, enc::ValType::I64)]);
        body.instruction(&enc::Instruction::End);
        code.function(&body);

        let mut module = enc::Module::new();
        module.section(&types);
        module.section(&imports);
        module.section(&funcs);
        module.section(&memories);
        module.section(&exports);
        module.section(&code);
        module.finish()
    }

    #[test]
    fn rejects_an_absurd_local_declaration() {
        // Locals are run-length encoded, so a few bytes can ask for billions of them. The
        // interpreter indexes locals directly and therefore expands the declaration, so
        // decoding has to bound it — otherwise a malicious workload exhausts memory before it
        // executes a single instruction.
        let ok = module_with_local_count(MAX_LOCALS_PER_FUNCTION);
        let image = decode(&ok).expect("the limit itself must be accepted");
        assert_eq!(
            image.function(image.entry).unwrap().local_types.len(),
            MAX_LOCALS_PER_FUNCTION as usize
        );

        let too_many = module_with_local_count(MAX_LOCALS_PER_FUNCTION + 1);
        assert_eq!(
            decode(&too_many).unwrap_err(),
            ImageError::TooManyLocals {
                function: 0,
                declared: u64::from(MAX_LOCALS_PER_FUNCTION) + 1,
            }
        );
    }

    #[test]
    fn keeps_element_segments_for_indirect_calls() {
        let module = bare(
            r#"(module
                 (memory (export "memory") 1 1)
                 (type $sig (func (result i32)))
                 (table 2 4 funcref)
                 (func $a (type $sig) (i32.const 1))
                 (func $b (type $sig) (i32.const 2))
                 (elem (i32.const 0) $a $b)
                 (func (export "cairn_run")
                   (drop (call_indirect (type $sig) (i32.const 0)))))"#,
        );
        let image = decode(&module).unwrap();

        assert_eq!(
            image.table,
            Some(TableSpec {
                initial: 2,
                maximum: Some(4)
            })
        );
        // Element function indices are remapped by the pass along with everything else:
        // $a and $b were 0 and 1, and become 1 and 2 behind the charge import.
        assert_eq!(
            image.elements,
            vec![ElementSegment::Active {
                offset: 0,
                functions: vec![1, 2]
            }]
        );
    }

    #[test]
    fn every_error_renders_a_message() {
        let samples = [
            ImageError::Parse {
                detail: "d".to_owned(),
            },
            ImageError::UnsupportedImport {
                module: "m".to_owned(),
                name: "n".to_owned(),
            },
            ImageError::BadMemory,
            ImageError::NoEntryPoint,
            ImageError::NotInstrumented,
            ImageError::UnsupportedInitializer,
            ImageError::TooManyLocals {
                function: 0,
                declared: 1,
            },
            ImageError::UnbalancedControl { function: 0 },
            ImageError::BodyWithoutSignature { index: 0 },
        ];
        for sample in samples {
            assert!(
                !sample.to_string().is_empty(),
                "empty message for {sample:?}"
            );
        }
    }
}
