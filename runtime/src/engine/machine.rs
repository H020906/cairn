//! The interpreter: machine state, control flow, and the step function arbitration calls.
//!
//! # `step` is the primitive, `run` is a loop over it
//!
//! Most interpreters are written as a run loop, with single-stepping bolted on later if
//! anyone asks. Here it is the other way round, because executing exactly one instruction
//! from a committed state *is* the operation dispute arbitration performs. Building the loop
//! on top of the step guarantees the two can never drift apart — there is only one
//! implementation of what an instruction does.
//!
//! # Dispatch order
//!
//! Every operator is offered to [`numeric::apply`] first. What it declines is control flow,
//! variables, memory or calls, handled here. Anything still unmatched becomes
//! [`Trap::Unsupported`], which is what makes "[`crate::validate`]'s allowlist equals the
//! interpreter's coverage" a property a test can check rather than a comment.
//!
//! # Branching
//!
//! A `br` to a `block` or `if` label leaves that construct: the label is popped and execution
//! resumes past its `end`. A `br` to a `loop` label **re-enters** it: execution resumes just
//! after the `loop` instruction and the label stays on the stack. The arity differs for the
//! same reason — a block branch carries the block's *results*, a loop branch carries its
//! *parameters*, because the loop body is about to run again and expects its inputs.

use std::collections::BTreeSet;

use wasmparser::{BlockType, Operator, ValType};

use crate::engine::image::{DataSegment, HostFunction, Image, MemorySpec};
use crate::engine::numeric::{self, Trap};
use crate::fuel::{Charge, Fuel, FuelMeter};
use crate::merkle::{Hash, PageTree, PAGE_SIZE};
use crate::state::{self, FrameDigest, LabelDigest, ProgramCounter, StateCommitment, Value};

/// Execution limits, taken from the work unit's manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Instruction ceiling. Exhausting it traps rather than running forever.
    pub fuel: u64,
    /// Snapshot every `2^interval` instructions. Lower means finer pre-committed bisection
    /// brackets and more overhead.
    pub snapshot_interval_log2: u8,
    /// Maximum call depth. Bounds interpreter memory against unbounded recursion, and does so
    /// identically on every machine — unlike a native stack overflow, which would not.
    pub max_call_depth: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            fuel: 1 << 32,
            snapshot_interval_log2: 16,
            max_call_depth: 1024,
        }
    }
}

/// What a single [`Machine::step`] accomplished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "a snapshot must be recorded and completion must stop the loop"]
pub enum Progress {
    /// An instruction executed; execution continues.
    Continued,
    /// A snapshot boundary was crossed. The caller must record [`Machine::commit`] before
    /// stepping again, labelled with the enclosed fuel value.
    Snapshot {
        /// The fuel value this snapshot is labelled with.
        at: Fuel,
    },
    /// The entry function returned.
    Finished,
}

/// A committed point in an execution.
///
/// # Why the step index and not the fuel value
///
/// Fuel is charged per basic block, so it advances in jumps: dozens of instructions between
/// two `charge` calls all share one fuel value. That makes "the state at fuel F" ambiguous,
/// and a bisection coordinate has to be unambiguous.
///
/// The step index has no such problem — [`Machine::step`] executes exactly one instruction, so
/// step *n* names exactly one state. Fuel is the budget mechanism; the step index is the
/// position mechanism. Both are recorded, and bisection searches the step index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snapshot {
    /// Instructions executed before this state.
    pub step: u64,
    /// Fuel consumed at this point. Carried for cost accounting, not for addressing.
    pub fuel: Fuel,
    /// The state root.
    pub root: Hash,
}

/// The artefact a worker submits: the answer, and a commitment to how it got there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trace {
    /// State root before the first instruction.
    pub initial: Hash,
    /// Committed states at each snapshot boundary, strictly increasing in step index.
    pub snapshots: Vec<Snapshot>,
    /// State root after the last instruction.
    pub final_root: Hash,
    /// Instructions executed.
    pub steps: u64,
    /// Fuel consumed.
    pub fuel: Fuel,
    /// What the workload wrote through `cairn.output`.
    pub output: Vec<u8>,
}

/// Linear memory: the bytes a program sees, and the commitment a verifier checks.
#[derive(Debug, Clone)]
struct Memory {
    bytes: Vec<u8>,
    pages: u32,
    max_pages: u32,
    /// Sized once to `max_pages`, so `memory.grow` never reshapes it. Pages past the current
    /// end read as zero; [`state::hash_memory`] binds the page count separately.
    tree: PageTree,
    /// Pages written since the last commitment. A snapshot rehashes only these.
    dirty: BTreeSet<u32>,
}

impl Memory {
    fn new(spec: MemorySpec) -> Self {
        let pages = spec.initial_pages;
        Self {
            bytes: vec![0; pages as usize * PAGE_SIZE],
            pages,
            max_pages: spec.maximum_pages,
            tree: PageTree::new(spec.maximum_pages.max(1) as usize),
            dirty: BTreeSet::new(),
        }
    }

    /// Byte range for an access, or a trap. Addresses are computed in 64 bits so that a base
    /// near `u32::MAX` plus a large static offset cannot wrap into a valid address.
    fn range(&self, address: u64, len: usize) -> Result<std::ops::Range<usize>, Trap> {
        let end = address
            .checked_add(len as u64)
            .ok_or(Trap::MemoryOutOfBounds)?;
        if end > self.bytes.len() as u64 {
            return Err(Trap::MemoryOutOfBounds);
        }
        // `end` is bounded by `bytes.len()`, so both casts are lossless.
        Ok(address as usize..end as usize)
    }

    fn read(&self, address: u64, len: usize) -> Result<&[u8], Trap> {
        let range = self.range(address, len)?;
        self.bytes.get(range).ok_or(Trap::MemoryOutOfBounds)
    }

    fn write(&mut self, address: u64, data: &[u8]) -> Result<(), Trap> {
        let range = self.range(address, data.len())?;
        let start = range.start;
        let slot = self.bytes.get_mut(range).ok_or(Trap::MemoryOutOfBounds)?;
        slot.copy_from_slice(data);
        self.mark_dirty(start, data.len());
        Ok(())
    }

    /// Fill a range with one byte, without materialising a buffer for it.
    fn fill(&mut self, address: u64, len: usize, byte: u8) -> Result<(), Trap> {
        let range = self.range(address, len)?;
        let start = range.start;
        let slot = self.bytes.get_mut(range).ok_or(Trap::MemoryOutOfBounds)?;
        slot.fill(byte);
        self.mark_dirty(start, len);
        Ok(())
    }

    /// Copy within memory. Ranges may overlap, so this goes through `copy_within`.
    fn copy(&mut self, dest: u64, src: u64, len: usize) -> Result<(), Trap> {
        let src_range = self.range(src, len)?;
        let dest_range = self.range(dest, len)?;
        self.bytes.copy_within(src_range, dest_range.start);
        self.mark_dirty(dest_range.start, len);
        Ok(())
    }

    fn mark_dirty(&mut self, start: usize, len: usize) {
        if len == 0 {
            return;
        }
        let first = start / PAGE_SIZE;
        let last = (start + len - 1) / PAGE_SIZE;
        for page in first..=last {
            self.dirty.insert(page as u32);
        }
    }

    /// Grow by `delta` pages, returning the previous size, or `-1` if it does not fit.
    ///
    /// Failure depends only on the module's declared maximum, never on how much memory the
    /// host happens to have — otherwise the same workload would grow successfully on one
    /// volunteer's machine and fail on another's.
    fn grow(&mut self, delta: u32) -> i32 {
        let Some(new_pages) = self.pages.checked_add(delta) else {
            return -1;
        };
        if new_pages > self.max_pages {
            return -1;
        }
        let previous = self.pages;
        self.bytes.resize(new_pages as usize * PAGE_SIZE, 0);
        self.pages = new_pages;
        // New pages are zero, which is what the tree already holds for them.
        previous as i32
    }

    fn commit(&mut self) -> Hash {
        for page in std::mem::take(&mut self.dirty) {
            let start = page as usize * PAGE_SIZE;
            if let Some(bytes) = self.bytes.get(start..start + PAGE_SIZE) {
                // The tree is sized to `max_pages`, so every live page index is in range.
                let _ = self.tree.set_page(page as usize, bytes);
            }
        }
        state::hash_memory(self.pages, &self.tree.root())
    }
}

/// A branch destination within a function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Label {
    branch_target: u32,
    arity: u32,
    stack_height: u32,
    /// Matching `end`, used when an `else` is reached by falling out of the then-branch.
    end: u32,
    is_loop: bool,
}

/// One activation of a function.
#[derive(Debug, Clone)]
struct Frame {
    function: u32,
    pc: u32,
    locals: Vec<Value>,
    stack_base: u32,
    arity: u32,
    labels: Vec<Label>,
}

/// An interpreter over one instrumented module.
#[derive(Debug, Clone)]
pub struct Machine<'a> {
    image: &'a Image<'a>,
    limits: Limits,
    memory: Memory,
    globals: Vec<Value>,
    stack: Vec<Value>,
    frames: Vec<Frame>,
    table: Vec<Option<u32>>,
    meter: FuelMeter,
    input: Vec<u8>,
    output: Vec<u8>,
    dropped_data: Vec<bool>,
    dropped_elements: Vec<bool>,
    /// Instructions executed. The coordinate dispute bisection searches — see [`Snapshot`].
    steps: u64,
    finished: bool,
}

impl<'a> Machine<'a> {
    /// Instantiate a module: install memory, globals and the table, then enter the entry
    /// point.
    ///
    /// # Errors
    ///
    /// Returns a [`Trap`] if a data or element segment does not fit, or if the limits are
    /// unusable.
    pub fn new(image: &'a Image<'a>, input: Vec<u8>, limits: Limits) -> Result<Self, Trap> {
        let meter = FuelMeter::new(limits.fuel, limits.snapshot_interval_log2)
            .map_err(|_| Trap::OutOfFuel)?;

        let mut memory = Memory::new(image.memory);
        for segment in &image.data {
            if let DataSegment::Active { offset, bytes } = segment {
                memory.write(u64::from(*offset), bytes)?;
            }
        }

        let table_size = image.table.map_or(0, |t| t.initial) as usize;
        let mut table = vec![None; table_size];
        for segment in &image.elements {
            if let crate::engine::image::ElementSegment::Active { offset, functions } = segment {
                for (i, function) in functions.iter().enumerate() {
                    let slot = (*offset as usize)
                        .checked_add(i)
                        .ok_or(Trap::TableOutOfBounds)?;
                    *table.get_mut(slot).ok_or(Trap::TableOutOfBounds)? = Some(*function);
                }
            }
        }

        let globals = image.globals.iter().map(|g| g.init).collect();
        let dropped_data = vec![false; image.data.len()];
        let dropped_elements = vec![false; image.elements.len()];

        let mut machine = Self {
            image,
            limits,
            memory,
            globals,
            stack: Vec::new(),
            frames: Vec::new(),
            table,
            meter,
            input,
            output: Vec::new(),
            dropped_data,
            dropped_elements,
            steps: 0,
            finished: false,
        };
        machine.enter(image.entry)?;
        Ok(machine)
    }

    /// The state root right now.
    pub fn commit(&mut self) -> StateCommitment {
        let memory = self.memory.commit();
        let frames: Vec<FrameDigest> = self
            .frames
            .iter()
            .map(|frame| FrameDigest {
                function: frame.function,
                instruction: frame.pc,
                stack_base: frame.stack_base,
                arity: frame.arity,
                locals: state::hash_values(&frame.locals),
                labels: state::hash_labels(
                    &frame
                        .labels
                        .iter()
                        .map(|l| LabelDigest {
                            branch_target: l.branch_target,
                            arity: l.arity,
                            stack_height: l.stack_height,
                            is_loop: l.is_loop,
                        })
                        .collect::<Vec<_>>(),
                ),
            })
            .collect();

        StateCommitment {
            memory,
            globals: state::hash_values(&self.globals),
            operand_stack: state::hash_values(&self.stack),
            call_stack: state::hash_frames(&frames),
            program_counter: self.program_counter(),
            fuel: self.meter.consumed(),
        }
    }

    /// Where execution has reached. Past the end this reports the entry point at instruction
    /// zero, so a finished execution still has a well-defined counter.
    fn program_counter(&self) -> ProgramCounter {
        self.frames.last().map_or(
            ProgramCounter {
                function: self.image.entry,
                instruction: 0,
            },
            |frame| ProgramCounter {
                function: frame.function,
                instruction: frame.pc,
            },
        )
    }

    /// Fuel consumed.
    #[must_use]
    pub fn fuel(&self) -> Fuel {
        self.meter.consumed()
    }

    /// Instructions executed.
    ///
    /// This is the coordinate dispute bisection addresses states by, because it names exactly
    /// one state per value where a fuel value does not. See [`Snapshot`].
    #[must_use]
    pub fn steps(&self) -> u64 {
        self.steps
    }

    /// What the workload has written through `cairn.output`.
    #[must_use]
    pub fn output(&self) -> &[u8] {
        &self.output
    }

    /// Whether the entry function has returned.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Run to completion, recording a state root at every snapshot boundary.
    ///
    /// # Errors
    ///
    /// Returns the [`Trap`] that stopped execution.
    pub fn run(&mut self) -> Result<Trace, Trap> {
        let initial = self.commit().root();
        let mut snapshots = Vec::new();

        loop {
            match self.step()? {
                Progress::Continued => {}
                Progress::Snapshot { at } => {
                    let step = self.steps;
                    snapshots.push(Snapshot {
                        step,
                        fuel: at,
                        root: self.commit().root(),
                    });
                }
                Progress::Finished => break,
            }
        }

        Ok(Trace {
            initial,
            final_root: self.commit().root(),
            steps: self.steps,
            fuel: self.meter.consumed(),
            output: std::mem::take(&mut self.output),
            snapshots,
        })
    }

    /// Execute exactly one instruction.
    ///
    /// This is the operation arbitration performs, and the only place an instruction's meaning
    /// is defined.
    ///
    /// # Errors
    ///
    /// Returns the [`Trap`] the instruction produces.
    pub fn step(&mut self) -> Result<Progress, Trap> {
        if self.finished {
            return Ok(Progress::Finished);
        }

        // `image` is a shared reference with the machine's own lifetime, so copying it out
        // lets the operator be read while the machine is mutated.
        let image = self.image;

        let (function_index, pc) = {
            let frame = self.frames.last().ok_or(Trap::StackUnderflow)?;
            (frame.function, frame.pc)
        };
        let function = image
            .function(function_index)
            .ok_or(Trap::UninitializedElement)?;
        let op = function
            .ops
            .get(pc as usize)
            .ok_or(Trap::MemoryOutOfBounds)?;

        // Advance first so branches can overwrite the counter.
        if let Some(frame) = self.frames.last_mut() {
            frame.pc = pc.saturating_add(1);
        }
        // One instruction is about to execute, whatever it turns out to be.
        self.steps = self.steps.saturating_add(1);

        if numeric::apply(op, &mut self.stack)? {
            return Ok(Progress::Continued);
        }

        self.execute_non_numeric(op, function_index, pc)
    }

    /// Everything [`numeric::apply`] declined.
    #[expect(
        clippy::too_many_lines,
        reason = "one arm per non-numeric instruction; the dispatch reads best as one table"
    )]
    fn execute_non_numeric(
        &mut self,
        op: &Operator<'_>,
        function_index: u32,
        pc: u32,
    ) -> Result<Progress, Trap> {
        let image = self.image;
        let function = image
            .function(function_index)
            .ok_or(Trap::UninitializedElement)?;

        match op {
            Operator::Unreachable => return Err(Trap::Unreachable),
            Operator::Nop => {}

            // --- structured control -------------------------------------------------------
            Operator::Block { blockty } | Operator::Loop { blockty } => {
                let control = function
                    .control
                    .get(pc as usize)
                    .copied()
                    .flatten()
                    .ok_or(Trap::Unreachable)?;
                let (params, results) = self.block_arity(*blockty)?;
                let is_loop = matches!(op, Operator::Loop { .. });
                self.push_label(Label {
                    // A loop branch re-enters the body; a block branch leaves it.
                    branch_target: if is_loop {
                        pc.saturating_add(1)
                    } else {
                        control.end.saturating_add(1)
                    },
                    // ...and carries the parameters back in rather than the results out.
                    arity: if is_loop { params } else { results },
                    stack_height: (self.stack.len() as u32).saturating_sub(params),
                    end: control.end,
                    is_loop,
                })?;
            }

            Operator::If { blockty } => {
                let control = function
                    .control
                    .get(pc as usize)
                    .copied()
                    .flatten()
                    .ok_or(Trap::Unreachable)?;
                let (params, results) = self.block_arity(*blockty)?;
                let condition = self.pop_i32()?;

                if condition != 0 {
                    self.push_label(Label {
                        branch_target: control.end.saturating_add(1),
                        arity: results,
                        stack_height: (self.stack.len() as u32).saturating_sub(params),
                        end: control.end,
                        is_loop: false,
                    })?;
                } else if let Some(otherwise) = control.otherwise {
                    self.push_label(Label {
                        branch_target: control.end.saturating_add(1),
                        arity: results,
                        stack_height: (self.stack.len() as u32).saturating_sub(params),
                        end: control.end,
                        is_loop: false,
                    })?;
                    self.set_pc(otherwise.saturating_add(1));
                } else {
                    // No else branch and the condition is false: skip the construct entirely.
                    // Validation guarantees params equal results here, so the stack is already
                    // in the shape the code after `end` expects.
                    self.set_pc(control.end.saturating_add(1));
                }
            }

            Operator::Else => {
                // Reached by falling out of the then-branch. Jump to the matching `end`, which
                // pops the label.
                let end = self
                    .frames
                    .last()
                    .and_then(|f| f.labels.last())
                    .ok_or(Trap::StackUnderflow)?
                    .end;
                self.set_pc(end);
            }

            Operator::End => {
                let has_label = self
                    .frames
                    .last()
                    .is_some_and(|frame| !frame.labels.is_empty());
                if has_label {
                    if let Some(frame) = self.frames.last_mut() {
                        frame.labels.pop();
                    }
                } else {
                    // The function's own `end`.
                    return self.leave();
                }
            }

            Operator::Br { relative_depth } => self.branch(*relative_depth)?,
            Operator::BrIf { relative_depth } => {
                let condition = self.pop_i32()?;
                if condition != 0 {
                    self.branch(*relative_depth)?;
                }
            }
            Operator::BrTable { targets } => {
                let index = self.pop_i32()? as u32;
                let chosen = targets
                    .targets()
                    .nth(index as usize)
                    .transpose()
                    .map_err(|_| Trap::Unreachable)?
                    .unwrap_or_else(|| targets.default());
                self.branch(chosen)?;
            }
            Operator::Return => return self.leave(),

            // --- calls --------------------------------------------------------------------
            Operator::Call { function_index } => return self.call(*function_index),
            Operator::CallIndirect { type_index, .. } => {
                let slot = self.pop_i32()? as u32;
                let target = self
                    .table
                    .get(slot as usize)
                    .copied()
                    .ok_or(Trap::TableOutOfBounds)?
                    .ok_or(Trap::UninitializedElement)?;

                let expected = image
                    .types
                    .get(*type_index as usize)
                    .ok_or(Trap::SignatureMismatch)?;
                let actual = image.signature(target).ok_or(Trap::SignatureMismatch)?;
                if expected.params() != actual.params() || expected.results() != actual.results() {
                    return Err(Trap::SignatureMismatch);
                }
                return self.call(target);
            }

            // --- parametric ---------------------------------------------------------------
            Operator::Drop => {
                self.stack.pop().ok_or(Trap::StackUnderflow)?;
            }
            Operator::Select | Operator::TypedSelect { .. } => {
                let condition = self.pop_i32()?;
                let alternative = self.stack.pop().ok_or(Trap::StackUnderflow)?;
                let consequent = self.stack.pop().ok_or(Trap::StackUnderflow)?;
                self.stack.push(if condition != 0 {
                    consequent
                } else {
                    alternative
                });
            }

            // --- variables ----------------------------------------------------------------
            Operator::LocalGet { local_index } => {
                let value = *self
                    .frames
                    .last()
                    .and_then(|f| f.locals.get(*local_index as usize))
                    .ok_or(Trap::StackUnderflow)?;
                self.stack.push(value);
            }
            Operator::LocalSet { local_index } => {
                let value = self.stack.pop().ok_or(Trap::StackUnderflow)?;
                *self
                    .frames
                    .last_mut()
                    .and_then(|f| f.locals.get_mut(*local_index as usize))
                    .ok_or(Trap::StackUnderflow)? = value;
            }
            Operator::LocalTee { local_index } => {
                let value = *self.stack.last().ok_or(Trap::StackUnderflow)?;
                *self
                    .frames
                    .last_mut()
                    .and_then(|f| f.locals.get_mut(*local_index as usize))
                    .ok_or(Trap::StackUnderflow)? = value;
            }
            Operator::GlobalGet { global_index } => {
                let value = *self
                    .globals
                    .get(*global_index as usize)
                    .ok_or(Trap::StackUnderflow)?;
                self.stack.push(value);
            }
            Operator::GlobalSet { global_index } => {
                let value = self.stack.pop().ok_or(Trap::StackUnderflow)?;
                *self
                    .globals
                    .get_mut(*global_index as usize)
                    .ok_or(Trap::StackUnderflow)? = value;
            }

            // --- memory -------------------------------------------------------------------
            Operator::MemorySize { .. } => {
                self.stack.push(Value::I32(self.memory.pages as i32));
            }
            Operator::MemoryGrow { .. } => {
                let delta = self.pop_i32()? as u32;
                let previous = self.memory.grow(delta);
                self.stack.push(Value::I32(previous));
            }
            Operator::MemoryFill { .. } => {
                let len = self.pop_i32()? as u32;
                let byte = self.pop_i32()? as u8;
                let address = u64::from(self.pop_i32()? as u32);
                self.memory.fill(address, len as usize, byte)?;
            }
            Operator::MemoryCopy { .. } => {
                let len = self.pop_i32()? as u32;
                let src = u64::from(self.pop_i32()? as u32);
                let dest = u64::from(self.pop_i32()? as u32);
                self.memory.copy(dest, src, len as usize)?;
            }
            Operator::MemoryInit { data_index, .. } => {
                let len = self.pop_i32()? as u32 as usize;
                let src = self.pop_i32()? as u32 as usize;
                let dest = u64::from(self.pop_i32()? as u32);

                let dropped = *self
                    .dropped_data
                    .get(*data_index as usize)
                    .ok_or(Trap::MemoryOutOfBounds)?;
                let bytes: &[u8] = if dropped {
                    &[]
                } else {
                    match image.data.get(*data_index as usize) {
                        Some(
                            DataSegment::Active { bytes, .. } | DataSegment::Passive { bytes },
                        ) => bytes,
                        None => return Err(Trap::MemoryOutOfBounds),
                    }
                };
                let slice = bytes
                    .get(src..src.checked_add(len).ok_or(Trap::MemoryOutOfBounds)?)
                    .ok_or(Trap::MemoryOutOfBounds)?
                    .to_vec();
                self.memory.write(dest, &slice)?;
            }
            Operator::DataDrop { data_index } => {
                *self
                    .dropped_data
                    .get_mut(*data_index as usize)
                    .ok_or(Trap::MemoryOutOfBounds)? = true;
            }
            Operator::ElemDrop { elem_index } => {
                *self
                    .dropped_elements
                    .get_mut(*elem_index as usize)
                    .ok_or(Trap::TableOutOfBounds)? = true;
            }

            _ => {
                if let Some(progress) = self.load_or_store(op)? {
                    return Ok(progress);
                }
                return Err(Trap::Unsupported {
                    operator: format!("{op:?}"),
                });
            }
        }

        Ok(Progress::Continued)
    }

    // --- helpers ---------------------------------------------------------------------------

    fn set_pc(&mut self, pc: u32) {
        if let Some(frame) = self.frames.last_mut() {
            frame.pc = pc;
        }
    }

    fn pop_i32(&mut self) -> Result<i32, Trap> {
        match self.stack.pop().ok_or(Trap::StackUnderflow)? {
            Value::I32(v) => Ok(v),
            _ => Err(Trap::TypeMismatch),
        }
    }

    fn push_label(&mut self, label: Label) -> Result<(), Trap> {
        self.frames
            .last_mut()
            .ok_or(Trap::StackUnderflow)?
            .labels
            .push(label);
        Ok(())
    }

    /// Parameter and result counts of a block type.
    fn block_arity(&self, blockty: BlockType) -> Result<(u32, u32), Trap> {
        Ok(match blockty {
            BlockType::Empty => (0, 0),
            BlockType::Type(_) => (0, 1),
            BlockType::FuncType(index) => {
                let ty = self
                    .image
                    .types
                    .get(index as usize)
                    .ok_or(Trap::SignatureMismatch)?;
                (ty.params().len() as u32, ty.results().len() as u32)
            }
        })
    }

    /// Take a branch to the label `depth` entries from the top.
    fn branch(&mut self, depth: u32) -> Result<(), Trap> {
        let frame = self.frames.last().ok_or(Trap::StackUnderflow)?;
        let index = frame
            .labels
            .len()
            .checked_sub(depth as usize + 1)
            .ok_or(Trap::StackUnderflow)?;
        let label = *frame.labels.get(index).ok_or(Trap::StackUnderflow)?;

        // Preserve the values the label carries, discard the rest of the block's stack.
        let keep = self
            .stack
            .len()
            .checked_sub(label.arity as usize)
            .ok_or(Trap::StackUnderflow)?;
        let preserved: Vec<Value> = self.stack.split_off(keep);
        self.stack.truncate(label.stack_height as usize);
        self.stack.extend(preserved);

        let frame = self.frames.last_mut().ok_or(Trap::StackUnderflow)?;
        // A loop is re-entered, so its label survives the branch; a block is left.
        frame
            .labels
            .truncate(if label.is_loop { index + 1 } else { index });
        frame.pc = label.branch_target;
        Ok(())
    }

    /// Push a frame for `function_index`, or run the host function it names.
    fn call(&mut self, function_index: u32) -> Result<Progress, Trap> {
        if function_index == self.image.charge {
            return self.charge();
        }
        if let Some(host) = self.image.host_function(function_index) {
            self.host_call(host)?;
            return Ok(Progress::Continued);
        }
        self.enter(function_index)?;
        Ok(Progress::Continued)
    }

    /// Enter a defined function: bind parameters as locals, zero the declared ones.
    fn enter(&mut self, function_index: u32) -> Result<(), Trap> {
        if self.frames.len() as u32 >= self.limits.max_call_depth {
            return Err(Trap::CallStackExhausted);
        }

        let signature = self
            .image
            .signature(function_index)
            .ok_or(Trap::SignatureMismatch)?;
        let params = signature.params().len();
        let results = signature.results().len() as u32;

        let body = self
            .image
            .function(function_index)
            .ok_or(Trap::UninitializedElement)?;

        let split = self
            .stack
            .len()
            .checked_sub(params)
            .ok_or(Trap::StackUnderflow)?;
        let mut locals: Vec<Value> = self.stack.split_off(split);
        locals.extend(body.local_types.iter().map(|ty| zero_of(*ty)));

        self.frames.push(Frame {
            function: function_index,
            pc: 0,
            locals,
            stack_base: self.stack.len() as u32,
            arity: results,
            labels: Vec::new(),
        });
        Ok(())
    }

    /// Return from the current function.
    fn leave(&mut self) -> Result<Progress, Trap> {
        let frame = self.frames.pop().ok_or(Trap::StackUnderflow)?;

        let keep = self
            .stack
            .len()
            .checked_sub(frame.arity as usize)
            .ok_or(Trap::StackUnderflow)?;
        let results: Vec<Value> = self.stack.split_off(keep);
        self.stack.truncate(frame.stack_base as usize);
        self.stack.extend(results);

        if self.frames.is_empty() {
            self.finished = true;
            return Ok(Progress::Finished);
        }
        Ok(Progress::Continued)
    }

    /// The fuel meter and snapshot hook, injected by [`crate::canon`].
    ///
    /// Intercepted rather than dispatched as a host call: a workload that could reach this
    /// directly could lie about how much it had executed, which is why `validate` rejects a
    /// module importing it.
    fn charge(&mut self) -> Result<Progress, Trap> {
        let instructions = self.pop_i32()? as u32;
        match self.meter.charge(u64::from(instructions)) {
            Charge::Continue => Ok(Progress::Continued),
            Charge::SnapshotDue { at } => Ok(Progress::Snapshot { at }),
            Charge::Exhausted { .. } => Err(Trap::OutOfFuel),
        }
    }

    /// Pop the base address and add the instruction's static offset.
    ///
    /// The sum is computed in 64 bits and checked, so a base near `u32::MAX` plus a large
    /// offset traps instead of wrapping into a valid-looking address.
    fn effective_address(&mut self, memarg: &wasmparser::MemArg) -> Result<u64, Trap> {
        let base = self.pop_i32()? as u32;
        u64::from(base)
            .checked_add(memarg.offset)
            .ok_or(Trap::MemoryOutOfBounds)
    }

    fn pop_i64(&mut self) -> Result<i64, Trap> {
        match self.stack.pop().ok_or(Trap::StackUnderflow)? {
            Value::I64(v) => Ok(v),
            _ => Err(Trap::TypeMismatch),
        }
    }

    fn pop_f32_bits(&mut self) -> Result<u32, Trap> {
        match self.stack.pop().ok_or(Trap::StackUnderflow)? {
            Value::F32(bits) => Ok(bits),
            _ => Err(Trap::TypeMismatch),
        }
    }

    fn pop_f64_bits(&mut self) -> Result<u64, Trap> {
        match self.stack.pop().ok_or(Trap::StackUnderflow)? {
            Value::F64(bits) => Ok(bits),
            _ => Err(Trap::TypeMismatch),
        }
    }

    /// The load and store instructions.
    ///
    /// Returns `Ok(None)` when the operator is not one, so [`execute_non_numeric`] can fall
    /// through to [`Trap::Unsupported`].
    ///
    /// Alignment hints in the memory argument are ignored on purpose: WebAssembly defines them
    /// as advisory, and an unaligned access is well-defined rather than a trap. Honouring them
    /// as constraints would reject valid programs.
    fn load_or_store(&mut self, op: &Operator<'_>) -> Result<Option<Progress>, Trap> {
        /// Read `$n` bytes and push what `$make` builds from them.
        macro_rules! load {
            ($memarg:expr, $n:literal, $make:expr) => {{
                let address = self.effective_address($memarg)?;
                let mut buf = [0u8; $n];
                buf.copy_from_slice(self.memory.read(address, $n)?);
                self.stack.push($make(buf));
                return Ok(Some(Progress::Continued));
            }};
        }

        /// Pop a value with `$pop`, then the address, and write `$bytes` of it.
        ///
        /// The order matters: a store pushes the address before the value, so the value comes
        /// off the stack first.
        macro_rules! store {
            ($memarg:expr, $pop:ident, $bytes:expr) => {{
                let value = self.$pop()?;
                let address = self.effective_address($memarg)?;
                self.memory.write(address, &$bytes(value))?;
                return Ok(Some(Progress::Continued));
            }};
        }

        match op {
            // --- full-width loads ---------------------------------------------------------
            Operator::I32Load { memarg } => load!(memarg, 4, |b| Value::I32(i32::from_le_bytes(b))),
            Operator::I64Load { memarg } => load!(memarg, 8, |b| Value::I64(i64::from_le_bytes(b))),
            Operator::F32Load { memarg } => load!(memarg, 4, |b| Value::F32(u32::from_le_bytes(b))),
            Operator::F64Load { memarg } => load!(memarg, 8, |b| Value::F64(u64::from_le_bytes(b))),

            // --- narrow loads -------------------------------------------------------------
            // The signed and unsigned forms differ only in how the narrow value widens, and
            // confusing them is silent: both produce a plausible number.
            Operator::I32Load8S { memarg } => {
                load!(memarg, 1, |b: [u8; 1]| Value::I32(i32::from(b[0] as i8)));
            }
            Operator::I32Load8U { memarg } => {
                load!(memarg, 1, |b: [u8; 1]| Value::I32(i32::from(b[0])));
            }
            Operator::I32Load16S { memarg } => {
                load!(memarg, 2, |b| Value::I32(i32::from(i16::from_le_bytes(b))));
            }
            Operator::I32Load16U { memarg } => {
                load!(memarg, 2, |b| Value::I32(i32::from(u16::from_le_bytes(b))));
            }
            Operator::I64Load8S { memarg } => {
                load!(memarg, 1, |b: [u8; 1]| Value::I64(i64::from(b[0] as i8)));
            }
            Operator::I64Load8U { memarg } => {
                load!(memarg, 1, |b: [u8; 1]| Value::I64(i64::from(b[0])));
            }
            Operator::I64Load16S { memarg } => {
                load!(memarg, 2, |b| Value::I64(i64::from(i16::from_le_bytes(b))));
            }
            Operator::I64Load16U { memarg } => {
                load!(memarg, 2, |b| Value::I64(i64::from(u16::from_le_bytes(b))));
            }
            Operator::I64Load32S { memarg } => {
                load!(memarg, 4, |b| Value::I64(i64::from(i32::from_le_bytes(b))));
            }
            Operator::I64Load32U { memarg } => {
                load!(memarg, 4, |b| Value::I64(i64::from(u32::from_le_bytes(b))));
            }

            // --- stores -------------------------------------------------------------------
            Operator::I32Store { memarg } => store!(memarg, pop_i32, i32::to_le_bytes),
            Operator::I64Store { memarg } => store!(memarg, pop_i64, i64::to_le_bytes),
            Operator::F32Store { memarg } => store!(memarg, pop_f32_bits, u32::to_le_bytes),
            Operator::F64Store { memarg } => store!(memarg, pop_f64_bits, u64::to_le_bytes),

            // Narrow stores keep the low bytes and discard the rest.
            Operator::I32Store8 { memarg } => store!(memarg, pop_i32, |v: i32| [v as u8]),
            Operator::I32Store16 { memarg } => {
                store!(memarg, pop_i32, |v: i32| (v as u16).to_le_bytes());
            }
            Operator::I64Store8 { memarg } => store!(memarg, pop_i64, |v: i64| [v as u8]),
            Operator::I64Store16 { memarg } => {
                store!(memarg, pop_i64, |v: i64| (v as u16).to_le_bytes());
            }
            Operator::I64Store32 { memarg } => {
                store!(memarg, pop_i64, |v: i64| (v as u32).to_le_bytes());
            }

            _ => Ok(None),
        }
    }

    fn host_call(&mut self, host: HostFunction) -> Result<(), Trap> {
        match host {
            HostFunction::Input => {
                let len = self.pop_i32()? as u32 as usize;
                let address = u64::from(self.pop_i32()? as u32);
                let available = self.input.len().min(len);
                if available > 0 {
                    let slice = self
                        .input
                        .get(..available)
                        .ok_or(Trap::MemoryOutOfBounds)?
                        .to_vec();
                    self.memory.write(address, &slice)?;
                }
                // The full length is returned so a workload can size its buffer with a
                // zero-length probe.
                self.stack.push(Value::I32(self.input.len() as i32));
            }
            HostFunction::Output => {
                let len = self.pop_i32()? as u32 as usize;
                let address = u64::from(self.pop_i32()? as u32);
                self.output = self.memory.read(address, len)?.to_vec();
            }
            HostFunction::Charge => return Err(Trap::Unreachable),
        }
        Ok(())
    }
}

/// The value a freshly declared local holds.
const fn zero_of(ty: ValType) -> Value {
    match ty {
        ValType::I64 => Value::I64(0),
        ValType::F32 => Value::F32(0),
        ValType::F64 => Value::F64(0),
        // I32 and any type the validator would have rejected.
        _ => Value::I32(0),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use super::*;
    use crate::canon::{self, Config};
    use crate::engine::image;

    /// Assemble, validate and instrument, exactly as a coordinator would before dispatching.
    ///
    /// The validation step is here to catch malformed *test* input. Without it an invalid
    /// module reaches the interpreter and surfaces as a bare `StackUnderflow` from somewhere
    /// deep in execution, which is an expensive way to learn that a test's WebAssembly text
    /// was wrong.
    fn canonical(text: &str) -> Vec<u8> {
        let source = wat::parse_str(text).expect("test module should assemble");
        crate::validate::validate_submitted(&source, crate::validate::Limits::default())
            .expect("test module should be a valid Cairn workload");
        canon::instrument(&source, Config::default()).expect("instrumentation should succeed")
    }

    fn run(module: &[u8], input: &[u8]) -> Result<Trace, Trap> {
        let image = image::decode(module).expect("instrumented module should decode");
        let mut machine = Machine::new(&image, input.to_vec(), Limits::default())?;
        machine.run()
    }

    /// Wrap a function body in a module that writes an i32 result to `output`.
    fn module_returning_i32(body: &str) -> String {
        format!(
            r#"(module
                 (import "cairn" "output" (func $output (param i32 i32)))
                 (memory (export "memory") 1 4)
                 (func $compute (result i32) {body})
                 (func (export "cairn_run")
                   (i32.store (i32.const 0) (call $compute))
                   (call $output (i32.const 0) (i32.const 4))))"#
        )
    }

    fn eval_i32(body: &str) -> i32 {
        let module = canonical(&module_returning_i32(body));
        let trace = run(&module, &[]).expect("execution should succeed");
        assert_eq!(trace.output.len(), 4, "expected four output bytes");
        i32::from_le_bytes([
            trace.output[0],
            trace.output[1],
            trace.output[2],
            trace.output[3],
        ])
    }

    #[test]
    fn evaluates_arithmetic_end_to_end() {
        assert_eq!(eval_i32("(i32.add (i32.const 20) (i32.const 22))"), 42);
        assert_eq!(eval_i32("(i32.mul (i32.const 6) (i32.const 7))"), 42);
    }

    #[test]
    fn runs_a_loop_to_completion() {
        // Sum 1..=10. Exercises loop labels, br_if and locals together.
        assert_eq!(
            eval_i32(
                r#"(local $i i32) (local $sum i32)
                   (local.set $i (i32.const 1))
                   (block $done
                     (loop $again
                       (br_if $done (i32.gt_u (local.get $i) (i32.const 10)))
                       (local.set $sum (i32.add (local.get $sum) (local.get $i)))
                       (local.set $i (i32.add (local.get $i) (i32.const 1)))
                       (br $again)))
                   (local.get $sum)"#
            ),
            55
        );
    }

    #[test]
    fn branches_to_a_loop_label_re_enter_it() {
        // The distinction that is easy to get backwards: a br to a loop label jumps to the
        // start of the body and keeps the label, while a br to a block label leaves. If loop
        // labels were popped like block labels, the second iteration would branch into
        // nothing.
        assert_eq!(
            eval_i32(
                r#"(local $n i32)
                   (local.set $n (i32.const 3))
                   (loop $again
                     (local.set $n (i32.sub (local.get $n) (i32.const 1)))
                     (br_if $again (local.get $n)))
                   (local.get $n)"#
            ),
            0
        );
    }

    #[test]
    fn if_else_takes_both_paths() {
        assert_eq!(
            eval_i32("(if (result i32) (i32.const 1) (then (i32.const 10)) (else (i32.const 20)))"),
            10
        );
        assert_eq!(
            eval_i32("(if (result i32) (i32.const 0) (then (i32.const 10)) (else (i32.const 20)))"),
            20
        );
    }

    #[test]
    fn an_if_without_an_else_falls_through() {
        assert_eq!(
            eval_i32(
                r#"(local $x i32)
                   (local.set $x (i32.const 5))
                   (if (i32.const 0) (then (local.set $x (i32.const 99))))
                   (local.get $x)"#
            ),
            5
        );
    }

    #[test]
    fn br_table_selects_its_target_and_falls_back_to_the_default() {
        // Three nested blocks, each branch landing just past a different `end`. Every block
        // is result-free so the dispatch is tested without stack juggling on top.
        let dispatch = |index: i32| {
            eval_i32(&format!(
                r#"(local $r i32)
                   (block $default
                     (block $one
                       (block $zero
                         (br_table $zero $one $default (i32.const {index})))
                       (local.set $r (i32.const 100))
                       (br $default))
                     (local.set $r (i32.const 200))
                     (br $default))
                   (local.get $r)"#
            ))
        };
        assert_eq!(dispatch(0), 100, "index 0 selects the innermost label");
        assert_eq!(dispatch(1), 200, "index 1 selects the middle label");
        assert_eq!(
            dispatch(7),
            0,
            "an index past the table takes the default and skips both assignments"
        );
    }

    #[test]
    fn calls_nest_and_return() {
        let module = canonical(
            r#"(module
                 (import "cairn" "output" (func $output (param i32 i32)))
                 (memory (export "memory") 1 4)
                 (func $double (param $x i32) (result i32)
                   (i32.mul (local.get $x) (i32.const 2)))
                 (func $quadruple (param $x i32) (result i32)
                   (call $double (call $double (local.get $x))))
                 (func (export "cairn_run")
                   (i32.store (i32.const 0) (call $quadruple (i32.const 5)))
                   (call $output (i32.const 0) (i32.const 4))))"#,
        );
        let trace = run(&module, &[]).unwrap();
        assert_eq!(trace.output, 20i32.to_le_bytes());
    }

    #[test]
    fn recursion_returns_the_right_answer() {
        let module = canonical(
            r#"(module
                 (import "cairn" "output" (func $output (param i32 i32)))
                 (memory (export "memory") 1 4)
                 (func $fact (param $n i32) (result i32)
                   (if (result i32) (i32.le_u (local.get $n) (i32.const 1))
                     (then (i32.const 1))
                     (else (i32.mul (local.get $n)
                                    (call $fact (i32.sub (local.get $n) (i32.const 1)))))))
                 (func (export "cairn_run")
                   (i32.store (i32.const 0) (call $fact (i32.const 10)))
                   (call $output (i32.const 0) (i32.const 4))))"#,
        );
        let trace = run(&module, &[]).unwrap();
        assert_eq!(trace.output, 3_628_800i32.to_le_bytes());
    }

    #[test]
    fn indirect_calls_dispatch_through_the_table() {
        let module = canonical(
            r#"(module
                 (import "cairn" "output" (func $output (param i32 i32)))
                 (memory (export "memory") 1 4)
                 (type $sig (func (result i32)))
                 (table 2 2 funcref)
                 (func $a (type $sig) (i32.const 111))
                 (func $b (type $sig) (i32.const 222))
                 (elem (i32.const 0) $a $b)
                 (func (export "cairn_run")
                   (i32.store (i32.const 0) (call_indirect (type $sig) (i32.const 1)))
                   (call $output (i32.const 0) (i32.const 4))))"#,
        );
        let trace = run(&module, &[]).unwrap();
        assert_eq!(trace.output, 222i32.to_le_bytes());
    }

    #[test]
    fn memory_round_trips_every_width() {
        assert_eq!(
            eval_i32(
                r#"(i32.store8 (i32.const 16) (i32.const 0xff))
                   (i32.load8_u (i32.const 16))"#
            ),
            255
        );
        assert_eq!(
            eval_i32(
                r#"(i32.store8 (i32.const 16) (i32.const 0xff))
                   (i32.load8_s (i32.const 16))"#
            ),
            -1,
            "the signed and unsigned narrow loads must differ"
        );
        assert_eq!(
            eval_i32(
                r#"(i64.store (i32.const 24) (i64.const 0x1122334455667788))
                   (i32.wrap_i64 (i64.load32_u (i32.const 24)))"#
            ),
            0x5566_7788_u32 as i32
        );
    }

    #[test]
    fn the_static_offset_is_added_to_the_base() {
        assert_eq!(
            eval_i32(
                r#"(i32.store offset=8 (i32.const 100) (i32.const 7))
                   (i32.load (i32.const 108))"#
            ),
            7
        );
    }

    #[test]
    fn data_segments_are_installed_before_execution() {
        let module = canonical(
            r#"(module
                 (import "cairn" "output" (func $output (param i32 i32)))
                 (memory (export "memory") 1 4)
                 (data (i32.const 0) "cairn")
                 (func (export "cairn_run")
                   (call $output (i32.const 0) (i32.const 5))))"#,
        );
        let trace = run(&module, &[]).unwrap();
        assert_eq!(trace.output, b"cairn");
    }

    #[test]
    fn input_is_delivered_and_its_true_length_reported() {
        let module = canonical(
            r#"(module
                 (import "cairn" "input"  (func $input  (param i32 i32) (result i32)))
                 (import "cairn" "output" (func $output (param i32 i32)))
                 (memory (export "memory") 1 4)
                 (func (export "cairn_run") (local $len i32)
                   ;; A zero-length probe reports the size without copying anything.
                   (local.set $len (call $input (i32.const 0) (i32.const 0)))
                   (drop (call $input (i32.const 64) (local.get $len)))
                   (call $output (i32.const 64) (local.get $len))))"#,
        );
        let trace = run(&module, b"hello volunteer").unwrap();
        assert_eq!(trace.output, b"hello volunteer");
    }

    #[test]
    fn memory_grows_within_its_declared_maximum() {
        // Growth must fail on the module's declared ceiling, never on how much memory the
        // host happens to have -- otherwise the same workload would succeed on one
        // volunteer's machine and fail on another's.
        assert_eq!(eval_i32("(memory.size)"), 1);
        assert_eq!(
            eval_i32("(memory.grow (i32.const 2))"),
            1,
            "returns the old size"
        );
        assert_eq!(
            eval_i32("(drop (memory.grow (i32.const 2))) (memory.size)"),
            3
        );
        assert_eq!(
            eval_i32("(memory.grow (i32.const 99))"),
            -1,
            "growth past the declared maximum fails"
        );
    }

    #[test]
    fn bulk_memory_fills_and_copies() {
        assert_eq!(
            eval_i32(
                r#"(memory.fill (i32.const 32) (i32.const 0xab) (i32.const 4))
                   (i32.load (i32.const 32))"#
            ),
            0xabab_abab_u32 as i32
        );
        assert_eq!(
            eval_i32(
                r#"(i32.store (i32.const 40) (i32.const 12345))
                   (memory.copy (i32.const 48) (i32.const 40) (i32.const 4))
                   (i32.load (i32.const 48))"#
            ),
            12345
        );
    }

    // --- traps -------------------------------------------------------------------------

    #[test]
    fn traps_propagate_out_of_execution() {
        let cases = [
            ("(unreachable)", Trap::Unreachable),
            (
                "(drop (i32.div_s (i32.const 1) (i32.const 0)))",
                Trap::DivideByZero,
            ),
            (
                "(drop (i32.load (i32.const 1000000)))",
                Trap::MemoryOutOfBounds,
            ),
            (
                "(i32.store (i32.const 1000000) (i32.const 1))",
                Trap::MemoryOutOfBounds,
            ),
        ];
        for (body, expected) in cases {
            let module = canonical(&format!(
                r#"(module
                     (memory (export "memory") 1 4)
                     (func (export "cairn_run") {body}))"#
            ));
            assert_eq!(run(&module, &[]).unwrap_err(), expected, "{body}");
        }
    }

    #[test]
    fn an_address_near_the_top_of_memory_traps_rather_than_wrapping() {
        // base + static offset is computed in 64 bits, so a base near u32::MAX plus a large
        // offset cannot wrap around into a valid-looking address.
        let module = canonical(
            r#"(module
                 (memory (export "memory") 1 4)
                 (func (export "cairn_run")
                   (drop (i32.load offset=4294967295 (i32.const 4294967295)))))"#,
        );
        assert_eq!(run(&module, &[]).unwrap_err(), Trap::MemoryOutOfBounds);
    }

    #[test]
    fn unbounded_recursion_exhausts_the_call_stack_deterministically() {
        // A native stack overflow would depend on the host's stack size. The limit is explicit
        // so the trap happens at the same call depth on every machine.
        let module = canonical(
            r#"(module
                 (memory (export "memory") 1 4)
                 (func $forever (call $forever))
                 (func (export "cairn_run") (call $forever)))"#,
        );
        let image = image::decode(&module).unwrap();
        let mut machine = Machine::new(
            &image,
            Vec::new(),
            Limits {
                max_call_depth: 32,
                ..Limits::default()
            },
        )
        .unwrap();
        assert_eq!(machine.run().unwrap_err(), Trap::CallStackExhausted);
    }

    #[test]
    fn running_out_of_fuel_traps() {
        let module = canonical(
            r#"(module
                 (memory (export "memory") 1 4)
                 (func (export "cairn_run") (local $i i32)
                   (loop $again
                     (local.set $i (i32.add (local.get $i) (i32.const 1)))
                     (br $again))))"#,
        );
        let image = image::decode(&module).unwrap();
        let mut machine = Machine::new(
            &image,
            Vec::new(),
            Limits {
                fuel: 500,
                ..Limits::default()
            },
        )
        .unwrap();
        assert_eq!(machine.run().unwrap_err(), Trap::OutOfFuel);
        assert!(machine.fuel().get() <= 500);
    }

    // --- the trace ---------------------------------------------------------------------

    #[test]
    fn snapshots_are_ordered_by_step_index() {
        // Bisection binary-searches this sequence, which is only meaningful if it is ordered.
        let module = canonical(
            r#"(module
                 (memory (export "memory") 1 4)
                 (func (export "cairn_run") (local $i i32)
                   (block $done
                     (loop $again
                       (br_if $done (i32.ge_u (local.get $i) (i32.const 5000)))
                       (local.set $i (i32.add (local.get $i) (i32.const 1)))
                       (br $again)))))"#,
        );
        let image = image::decode(&module).unwrap();
        let mut machine = Machine::new(
            &image,
            Vec::new(),
            Limits {
                // A small interval so a short program still produces several snapshots.
                snapshot_interval_log2: 8,
                ..Limits::default()
            },
        )
        .unwrap();
        let trace = machine.run().unwrap();

        assert!(
            trace.snapshots.len() > 4,
            "expected several snapshots, got {}",
            trace.snapshots.len()
        );
        assert!(
            trace.snapshots.is_sorted_by(|a, b| a.step < b.step),
            "step indices must strictly increase"
        );
        // Fuel is monotonic too, but only weakly: it is charged per basic block, so two
        // snapshots can share a value. That is exactly why the step index, and not fuel, is
        // what bisection addresses states by.
        assert!(trace.snapshots.is_sorted_by(|a, b| a.fuel <= b.fuel));
        assert_ne!(trace.initial, trace.final_root);
        assert_eq!(trace.steps, machine.steps());
    }

    #[test]
    fn execution_is_reproducible() {
        // The property everything else rests on: the same module and input produce the same
        // trace, byte for byte.
        let module = canonical(&module_returning_i32(
            r#"(local $i i32) (local $acc i32)
               (block $done
                 (loop $again
                   (br_if $done (i32.ge_u (local.get $i) (i32.const 100)))
                   (local.set $acc (i32.add (local.get $acc) (local.get $i)))
                   (local.set $i (i32.add (local.get $i) (i32.const 1)))
                   (br $again)))
               (local.get $acc)"#,
        ));
        let first = run(&module, b"seed").unwrap();
        let second = run(&module, b"seed").unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn different_inputs_reach_different_states() {
        let module = canonical(
            r#"(module
                 (import "cairn" "input"  (func $input  (param i32 i32) (result i32)))
                 (import "cairn" "output" (func $output (param i32 i32)))
                 (memory (export "memory") 1 4)
                 (func (export "cairn_run")
                   (drop (call $input (i32.const 0) (i32.const 8)))
                   (call $output (i32.const 0) (i32.const 8))))"#,
        );
        let a = run(&module, b"aaaaaaaa").unwrap();
        let b = run(&module, b"bbbbbbbb").unwrap();
        assert_ne!(a.final_root, b.final_root);
        assert_ne!(a.output, b.output);
    }

    #[test]
    fn stepping_and_running_agree() {
        // `run` is a loop over `step`, so a divergence here would mean two implementations of
        // what an instruction does -- exactly what building the loop on the step avoids.
        let module = canonical(&module_returning_i32(
            "(i32.add (i32.const 1) (i32.const 2))",
        ));
        let image = image::decode(&module).unwrap();

        let mut streamed = Machine::new(&image, Vec::new(), Limits::default()).unwrap();
        let expected = streamed.run().unwrap();

        let mut stepped = Machine::new(&image, Vec::new(), Limits::default()).unwrap();
        let initial = stepped.commit().root();
        let mut snapshots = Vec::new();
        loop {
            match stepped.step().unwrap() {
                Progress::Continued => {}
                Progress::Snapshot { at } => {
                    let step = stepped.steps();
                    snapshots.push(Snapshot {
                        step,
                        fuel: at,
                        root: stepped.commit().root(),
                    });
                }
                Progress::Finished => break,
            }
        }

        assert_eq!(initial, expected.initial);
        assert_eq!(snapshots, expected.snapshots);
        assert_eq!(stepped.commit().root(), expected.final_root);
        assert_eq!(stepped.steps(), expected.steps);
        assert_eq!(stepped.fuel(), expected.fuel);
        assert_eq!(stepped.output(), expected.output);
    }

    #[test]
    fn the_state_root_moves_with_every_kind_of_state() {
        // Memory, globals and the operand stack must each be visible in the commitment,
        // because a divergence in any of them has to be detectable.
        let module = canonical(
            r#"(module
                 (memory (export "memory") 1 4)
                 (global $g (mut i32) (i32.const 0))
                 (func (export "cairn_run")
                   (i32.store (i32.const 0) (i32.const 1))
                   (global.set $g (i32.const 2))))"#,
        );
        let image = image::decode(&module).unwrap();
        let mut machine = Machine::new(&image, Vec::new(), Limits::default()).unwrap();

        let mut seen = vec![machine.commit().root()];
        while !matches!(machine.step().unwrap(), Progress::Finished) {
            seen.push(machine.commit().root());
        }

        let unique: std::collections::BTreeSet<_> = seen.iter().collect();
        assert!(
            unique.len() > 3,
            "the state root barely moved across {} steps",
            seen.len()
        );
    }

    #[test]
    fn memory_growth_alone_changes_the_state_root() {
        // Grown-but-blank pages hold the same bytes as before, so only the page count tells
        // the two states apart. See state::hash_memory.
        let module = canonical(
            r#"(module
                 (memory (export "memory") 1 4)
                 (func (export "cairn_run") (drop (memory.grow (i32.const 1)))))"#,
        );
        let image = image::decode(&module).unwrap();
        let mut machine = Machine::new(&image, Vec::new(), Limits::default()).unwrap();

        let before = machine.commit().root();
        while !matches!(machine.step().unwrap(), Progress::Finished) {}
        assert_ne!(machine.commit().root(), before);
    }
}
