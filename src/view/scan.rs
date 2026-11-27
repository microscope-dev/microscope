//! Resumable nested-loop scanner for `sequence` and `map` view clauses.
//!
//! Executes `range`, `list`, `inorder`, and WebAssembly `kernel` generators with
//! cycle/depth protection and periodic cursor checkpointing for pagination.

use std::collections::BTreeSet;

use crate::ViewProblem;
use crate::eval::interp::{self, Value};
use crate::eval::number::{Exact, Integer};
use crate::eval::target::{Machine, Stop};

use super::bind::{BoundClause, BoundGenerator, BoundItem, BoundScan, ViewProgram};
use super::kernel::{self, Recordings, RunError};
use super::run::{Failure, ViewMachine, internal};

pub const CHECKPOINT_INTERVAL: u64 = 256;

pub const MAX_DEPTH: u32 = 128;

pub const MAX_UNCOUNTED: u64 = 1 << 24;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Var {
    Integer(i128),

    Node(u64),

    Words(Box<[u64]>),
}

fn exact<P>(value: impl Into<Exact>) -> Value<P> {
    Value::Int(Integer::Exact(value.into()))
}

impl Var {
    pub fn value<P>(&self) -> Value<P> {
        match self {
            Self::Integer(value) => exact(*value),
            Self::Node(address) => Value::Pointer(*address),
            Self::Words(words) => exact(words.first().copied().unwrap_or(0)),
        }
    }

    fn push<P>(&self, values: &mut Vec<Value<P>>) {
        match self {
            Self::Words(words) => values.extend(words.iter().map(|word| exact(*word))),
            _ => values.push(self.value()),
        }
    }
}

/// Brent's cycle detector complements the per-level `visited` set so corrupted
/// linked lists in target memory halt with `ViewProblem::Cycle` even across
/// checkpoint restores where `visited` starts empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Brent {
    tortoise: u64,
    power: u64,
    steps: u64,
}

impl Brent {
    const fn new(head: u64) -> Self {
        Self {
            tortoise: head,
            power: 1,
            steps: 1,
        }
    }

    const fn cycles(&mut self, node: u64) -> bool {
        if node == self.tortoise {
            return true;
        }

        if self.power == self.steps {
            self.tortoise = node;
            self.power = self.power.saturating_mul(2);
            self.steps = 0;
        }

        self.steps += 1;
        false
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Level {
    Range {
        next: u64,
        end: u64,
    },
    List {
        head: u64,

        next: Option<u64>,
        brent: Brent,

        repeats: bool,
    },
    Inorder {
        stack: Vec<u64>,

        pending: u64,
    },

    Kernel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    levels: Vec<Level>,
    variables: Vec<Var>,

    position: u64,

    done: bool,
}

impl Cursor {
    const fn start() -> Self {
        Self {
            levels: Vec::new(),
            variables: Vec::new(),
            position: 0,
            done: false,
        }
    }
}

/// Saved [`Cursor`] snapshots taken every [`CHECKPOINT_INTERVAL`] items.
///
/// Allows paging forward into large containers without rescanning from index 0.
/// Scans that include a Wasm `kernel` clause skip checkpointing because live
/// Wasm instances cannot be cloned.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Checkpoints {
    cursors: Vec<Cursor>,
    recordings: Option<Recordings>,
}

impl Checkpoints {
    pub const fn recording(recordings: Recordings) -> Self {
        Self {
            cursors: Vec::new(),
            recordings: Some(recordings),
        }
    }

    fn before(&self, index: u64) -> Cursor {
        self.cursors
            .iter()
            .rev()
            .find(|cursor| cursor.position <= index)
            .cloned()
            .unwrap_or_else(Cursor::start)
    }

    fn keep(&mut self, cursor: &Cursor) {
        debug_assert_eq!(
            cursor.variables.len(),
            cursor.levels.len(),
            "a checkpoint is kept before its last clause generated"
        );
        let expected = (self.cursors.len() as u64 + 1) * CHECKPOINT_INTERVAL;
        if cursor.position == expected {
            self.cursors.push(cursor.clone());
        }
    }
}

pub struct Scanner<'b, St, P> {
    scan: &'b BoundScan<St>,
    cursor: Cursor,

    values: Vec<Value<P>>,
    visited: Vec<BTreeSet<u64>>,

    runs: Vec<Option<kernel::Run>>,

    declared: Option<u64>,

    restored: bool,

    kernels: bool,
    recordings: Option<Recordings>,
}

impl<'b, St, P: Clone> Scanner<'b, St, P> {
    pub fn at(
        scan: &'b BoundScan<St>,
        declared: Option<u64>,
        checkpoints: &Checkpoints,
        index: u64,
    ) -> Self {
        let cursor = checkpoints.before(index);
        Self {
            scan,
            visited: vec![BTreeSet::new(); cursor.levels.len()],
            runs: (0..cursor.levels.len()).map(|_| None).collect(),
            restored: cursor.levels.is_empty(),
            values: Vec::new(),
            cursor,
            declared,
            kernels: scan
                .clauses
                .iter()
                .any(|clause| matches!(clause.generator, BoundGenerator::Kernel { .. })),
            recordings: checkpoints.recordings.clone(),
        }
    }

    pub const fn position(&self) -> u64 {
        self.cursor.position
    }

    fn base(&self, depth: usize) -> usize {
        self.scan
            .clauses
            .iter()
            .take(depth)
            .map(BoundClause::width)
            .sum()
    }

    pub fn skip_to<M: Machine<Step = St, Place = P>>(
        &mut self,
        index: u64,
        machine: &mut ViewMachine<'_, M>,
        checkpoints: &mut Checkpoints,
    ) -> Result<bool, Failure> {
        while self.cursor.position < index {
            if self.next(machine, checkpoints)?.is_none() {
                return Ok(false);
            }
        }

        Ok(true)
    }

    pub fn next<M: Machine<Step = St, Place = P>>(
        &mut self,
        machine: &mut ViewMachine<'_, M>,
        checkpoints: &mut Checkpoints,
    ) -> Result<Option<u64>, Failure> {
        if self.declared == Some(self.cursor.position) {
            return Ok(None);
        }

        if !self.generate(machine)? {
            if let Some(declared) = self.declared {
                return Err(Failure::Problem(ViewProblem::CountMismatch {
                    declared,
                    generated: self.cursor.position,
                }));
            }

            return Ok(None);
        }

        let index = self.cursor.position;
        self.cursor.position += 1;

        if self.declared.is_none() && self.cursor.position > MAX_UNCOUNTED {
            return Err(Failure::Problem(ViewProblem::TooMany {
                limit: MAX_UNCOUNTED,
            }));
        }

        if !self.kernels {
            checkpoints.keep(&self.cursor);
        }

        Ok(Some(index))
    }

    fn restore<M: Machine<Step = St, Place = P>>(
        &mut self,
        machine: &mut ViewMachine<'_, M>,
    ) -> Result<(), Failure> {
        self.values.clear();
        for depth in 0..self.cursor.levels.len() {
            let Some(variable) = self.cursor.variables.get(depth) else {
                return Err(internal("a checkpoint has a value for each level"));
            };

            variable.push(&mut self.values);
            if !self.items(depth, machine)? {
                return Err(internal(
                    "a checkpoint's values no longer pass their filters",
                ));
            }
        }

        self.restored = true;
        Ok(())
    }

    fn items<M: Machine<Step = St, Place = P>>(
        &mut self,
        depth: usize,
        machine: &mut ViewMachine<'_, M>,
    ) -> Result<bool, Failure> {
        let Some(clause) = self.scan.clauses.get(depth) else {
            return Err(internal("a scan has a clause at each depth it runs"));
        };

        for item in &clause.items {
            machine.set_variables(&self.values);
            match item {
                BoundItem::Filter(filter) => {
                    if !super::run::truth(filter, machine)? {
                        return Ok(false);
                    }
                }
                BoundItem::Let(program) => {
                    let value = interp::value(program, machine)?;
                    self.values.push(value);
                }
            }
        }

        machine.set_variables(&self.values);
        Ok(true)
    }

    fn generate<M: Machine<Step = St, Place = P>>(
        &mut self,
        machine: &mut ViewMachine<'_, M>,
    ) -> Result<bool, Failure> {
        if self.cursor.done {
            return Ok(false);
        }

        if !self.restored {
            self.restore(machine)?;
        }

        if self.cursor.levels.is_empty() {
            self.enter(0, machine)?;
        }

        loop {
            machine.charge()?;
            let depth = self.cursor.levels.len() - 1;
            let base = self.base(depth);
            self.values.truncate(base);
            let Some(value) = self.advance(depth, machine)? else {
                self.cursor.levels.pop();
                self.cursor.variables.truncate(depth);
                self.visited.truncate(depth);
                self.runs.truncate(depth);
                if depth == 0 {
                    self.cursor.done = true;
                    return Ok(false);
                }

                continue;
            };

            self.cursor.variables.truncate(depth);
            value.push(&mut self.values);
            self.cursor.variables.push(value);
            if !self.items(depth, machine)? {
                continue;
            }

            if depth + 1 == self.scan.clauses.len() {
                return Ok(true);
            }

            self.enter(depth + 1, machine)?;
        }
    }

    fn enter<M: Machine<Step = St, Place = P>>(
        &mut self,
        depth: usize,
        machine: &mut ViewMachine<'_, M>,
    ) -> Result<(), Failure> {
        let base = self.base(depth);
        self.values.truncate(base);
        machine.set_variables(&self.values);
        let mut run = None;
        let Some(clause) = self.scan.clauses.get(depth) else {
            return Err(internal("a scan has a clause at each depth it runs"));
        };

        let level = match &clause.generator {
            BoundGenerator::Range(length) => Level::Range {
                next: 0,
                end: super::run::count(length, machine, "range's length")?,
            },
            BoundGenerator::List { head, .. } => {
                let head = node(head, machine)?;
                Level::List {
                    head,
                    next: (head != 0).then_some(head),
                    brent: Brent::new(head),
                    repeats: false,
                }
            }
            BoundGenerator::Inorder { root, .. } => Level::Inorder {
                stack: Vec::new(),
                pending: node(root, machine)?,
            },
            BoundGenerator::Kernel {
                kernel, arguments, ..
            } => {
                let words = arguments
                    .iter()
                    .map(|argument| word(argument, machine))
                    .collect::<Result<Vec<_>, _>>()?;
                let started = kernel
                    .start(&words, self.recordings.as_ref())
                    .map_err(|reason| kernel_problem(kernel, reason))?;
                run = Some(started);
                Level::Kernel
            }
        };

        self.cursor.levels.push(level);
        self.visited.push(BTreeSet::new());
        self.runs.truncate(depth);
        self.runs.resize_with(depth, || None);
        self.runs.push(run);

        Ok(())
    }

    fn advance<M: Machine<Step = St, Place = P>>(
        &mut self,
        depth: usize,
        machine: &mut ViewMachine<'_, M>,
    ) -> Result<Option<Var>, Failure> {
        debug_assert_eq!(
            self.visited.len(),
            self.cursor.levels.len(),
            "each started level has one visited set"
        );
        let position = self.cursor.position;
        let outer = &mut self.values;
        let (Some(visited), Some(level), Some(clause)) = (
            self.visited.get_mut(depth),
            self.cursor.levels.get_mut(depth),
            self.scan.clauses.get(depth),
        ) else {
            return Err(internal("a started level has a clause and a visited set"));
        };

        match (level, &clause.generator) {
            (Level::Range { next, end }, _) => {
                if next < end {
                    let value = *next;
                    *next += 1;
                    Ok(Some(Var::Integer(i128::from(value))))
                } else {
                    Ok(None)
                }
            }

            (
                Level::List {
                    head,
                    next,
                    brent,
                    repeats,
                },
                BoundGenerator::List { next: link, .. },
            ) => {
                let Some(current) = *next else {
                    return Ok(None);
                };

                if *repeats || !visited.insert(current) {
                    return Err(Failure::Problem(ViewProblem::Cycle { at: position }));
                }

                let following = follow(link, outer, current, machine)?;
                if following == 0 || following == *head {
                    *next = None;
                } else {
                    *repeats = brent.cycles(following);
                    *next = Some(following);
                }

                Ok(Some(Var::Node(current)))
            }
            (Level::Inorder { stack, pending }, BoundGenerator::Inorder { left, right, .. }) => {
                while *pending != 0 {
                    machine.charge()?;
                    if !visited.insert(*pending) {
                        return Err(Failure::Problem(ViewProblem::Cycle { at: position }));
                    }

                    if stack.len() == MAX_DEPTH as usize {
                        return Err(Failure::Problem(ViewProblem::TooDeep { depth: MAX_DEPTH }));
                    }

                    stack.push(*pending);
                    *pending = follow(left, outer, *pending, machine)?;
                }

                let Some(current) = stack.pop() else {
                    return Ok(None);
                };
                *pending = follow(right, outer, current, machine)?;
                Ok(Some(Var::Node(current)))
            }
            (Level::Kernel, BoundGenerator::Kernel { kernel, words, .. }) => {
                let Some(Some(run)) = self.runs.get_mut(depth) else {
                    return Err(internal("a kernel's clause has no run"));
                };

                let item = run
                    .next(&mut Program { machine })
                    .map_err(|error| match error {
                        RunError::Host(stop) => Failure::from(stop),
                        RunError::Kernel(reason) => kernel_problem(kernel, reason),
                    })?;

                match item {
                    Some(item) if item.len() == *words => Ok(Some(Var::Words(item))),
                    Some(item) => Err(kernel_problem(
                        kernel,
                        format!(
                            "it yielded {} words, and the view names {words}",
                            item.len()
                        ),
                    )),
                    None => Ok(None),
                }
            }
            _ => Err(internal("a scan's state does not match its generator")),
        }
    }
}

struct Program<'v, 'm, M: Machine> {
    machine: &'v mut ViewMachine<'m, M>,
}

impl<M: Machine> kernel::Host for Program<'_, '_, M> {
    type Error = Stop;

    fn read(&mut self, address: u64, length: usize) -> Result<Vec<u8>, Stop> {
        self.machine.read(address, length)
    }

    fn pay(&mut self, units: u64) -> Result<(), Stop> {
        for _ in 0..units {
            self.machine.charge()?;
        }

        Ok(())
    }
}

fn kernel_problem(kernel: &kernel::Kernel, reason: String) -> Failure {
    Failure::Problem(ViewProblem::Kernel {
        kernel: std::sync::Arc::clone(kernel.name()),
        reason: reason.into(),
    })
}

fn word<M: Machine>(
    program: &ViewProgram<M::Step>,
    machine: &mut ViewMachine<'_, M>,
) -> Result<u64, Failure> {
    match interp::value(program, machine)? {
        Value::Int(integer) => {
            let value = integer.value();

            value
                .to_i128()
                .and_then(|value| {
                    u64::try_from(value)
                        .ok()
                        .or_else(|| i64::try_from(value).ok().map(i64::cast_unsigned))
                })
                .ok_or_else(|| {
                    Failure::Problem(ViewProblem::Refused(
                        format!("a kernel's argument is {value}, which is not 64 bits").into(),
                    ))
                })
        }
        Value::Pointer(address) => Ok(address),
        Value::Bool(value) => Ok(u64::from(value)),
        _ => Err(internal("a kernel's argument is not a word")),
    }
}

fn node<M: Machine>(
    program: &ViewProgram<M::Step>,
    machine: &mut ViewMachine<'_, M>,
) -> Result<u64, Failure> {
    match interp::value(program, machine)? {
        Value::Pointer(address) => Ok(address),
        _ => Err(internal("a node is not a pointer")),
    }
}

fn follow<M: Machine>(
    link: &ViewProgram<M::Step>,
    outer: &mut Vec<Value<M::Place>>,
    current: u64,
    machine: &mut ViewMachine<'_, M>,
) -> Result<u64, Failure> {
    outer.push(Value::Pointer(current));
    machine.set_variables(outer);
    let next = node(link, machine);
    outer.pop();
    next
}
