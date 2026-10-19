//! Generating a sequence's elements, or a map's entries, from nested
//! clauses: ranges, linked lists, and binary trees, each filtered
//! (`docs/views.md`).
//!
//! A scan's state is plain data, a [`Cursor`], so a scan can stop and later
//! resume from where it was: the controller keeps a cursor every
//! [`CHECKPOINT_INTERVAL`] elements for the current stop, and a later page
//! of children resumes from the nearest one rather than starting again.
//!
//! A scan never shows a node twice as if it were two elements. A list ends
//! at a null pointer or at its first node again; a node that comes back
//! otherwise is a cycle, found exactly among the nodes one run visits and
//! by Brent's algorithm across runs. A tree may be at most [`MAX_DEPTH`]
//! levels deep, and a scan without a declared count may generate at most
//! [`MAX_UNCOUNTED`] elements. Every step is charged to the inspection's
//! budget, so every scan ends.

use std::collections::BTreeSet;

use crate::ViewProblem;
use crate::eval::interp::{self, Value};
use crate::eval::number::{Exact, Integer};
use crate::eval::target::{Machine, Stop};

use super::bind::{BoundClause, BoundGenerator, BoundItem, BoundScan, ViewProgram};
use super::kernel::{self, Recordings, RunError};
use super::run::{Failure, ViewMachine, internal};

/// How many elements apart the controller keeps cursors.
pub const CHECKPOINT_INTERVAL: u64 = 256;

/// How deep a tree a view walks may be. A red-black tree of 2^24 nodes is
/// at most 48 levels deep.
pub const MAX_DEPTH: u32 = 128;

/// The most elements a scan without a declared count generates.
pub const MAX_UNCOUNTED: u64 = 1 << 24;

/// The value a clause's variable holds, or its variables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Var {
    /// A range's position, or an index a client asked for.
    Integer(i128),

    /// A linked structure's node: a pointer's address.
    Node(u64),

    /// A kernel's item: a word for each variable.
    Words(Box<[u64]>),
}

fn exact<P>(value: impl Into<Exact>) -> Value<P> {
    Value::Int(Integer::Exact(value.into()))
}

impl Var {
    /// A single variable as the evaluator sees it.
    pub fn value<P>(&self) -> Value<P> {
        match self {
            Self::Integer(value) => exact(*value),
            Self::Node(address) => Value::Pointer(*address),
            Self::Words(words) => exact(words.first().copied().unwrap_or(0)),
        }
    }

    /// The variables as the evaluator sees them, appended to `values`.
    fn push<P>(&self, values: &mut Vec<Value<P>>) {
        match self {
            Self::Words(words) => values.extend(words.iter().map(|word| exact(*word))),
            _ => values.push(self.value()),
        }
    }
}

/// Brent's cycle detection over one list's nodes: the tortoise moves to
/// the hare at each power of two, so a cycle is found within a few times
/// its length and its lead-in, in constant space.
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

    /// Takes the next node; true when it closes a cycle.
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

/// One clause's generator, part way through.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Level {
    Range {
        next: u64,
        end: u64,
    },
    List {
        head: u64,

        /// The node to generate next; `None` once it is known to end.
        next: Option<u64>,
        brent: Brent,

        /// Whether Brent's algorithm found the next node already visited.
        repeats: bool,
    },
    Inorder {
        /// Nodes whose left subtrees have been entered, deepest last.
        stack: Vec<u64>,

        /// A subtree still to enter, or 0.
        pending: u64,
    },

    /// A kernel's run, which the scanner holds.
    Kernel,
}

/// Where a scan is: each started clause's generator and variable, and how
/// many elements it has generated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    levels: Vec<Level>,
    variables: Vec<Var>,

    /// Elements generated before the cursor.
    position: u64,

    /// Whether the generators are exhausted.
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

/// The cursors one value's scan has passed at this stop, every
/// [`CHECKPOINT_INTERVAL`] elements, in order, and where its kernels' runs
/// are recorded, if they are. A scan with a kernel keeps no cursors, since
/// a kernel's run cannot be resumed but only run again.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Checkpoints {
    cursors: Vec<Cursor>,
    recordings: Option<Recordings>,
}

impl Checkpoints {
    /// No checkpoints, with every kernel run recorded into `recordings`.
    pub const fn recording(recordings: Recordings) -> Self {
        Self {
            cursors: Vec::new(),
            recordings: Some(recordings),
        }
    }

    /// The latest cursor at or before element `index`.
    fn before(&self, index: u64) -> Cursor {
        self.cursors
            .iter()
            .rev()
            .find(|cursor| cursor.position <= index)
            .cloned()
            .unwrap_or_else(Cursor::start)
    }

    fn keep(&mut self, cursor: &Cursor) {
        // A restored checkpoint has a value for each of its levels.
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

/// A scan in progress: its cursor, the values at every position, which
/// add the clauses' `let`s to the cursor's variables, and the nodes each of
/// its linked clauses has visited since this run began.
pub struct Scanner<'b, St, P> {
    scan: &'b BoundScan<St>,
    cursor: Cursor,

    /// The variables and `let`s of the started clauses, by position.
    values: Vec<Value<P>>,
    visited: Vec<BTreeSet<u64>>,

    /// The kernel clauses' runs, by depth.
    runs: Vec<Option<kernel::Run>>,

    /// The count the view declares, which the scan never passes.
    declared: Option<u64>,

    /// Whether `values` holds the cursor's clauses' values yet; a cursor
    /// resumed from a checkpoint computes them again.
    restored: bool,

    /// Whether a clause is a kernel's, so the scan keeps no checkpoints.
    kernels: bool,
    recordings: Option<Recordings>,
}

impl<'b, St, P: Clone> Scanner<'b, St, P> {
    /// A scan resumed at the latest checkpoint at or before element
    /// `index`.
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

    /// How many elements the scan has generated.
    pub const fn position(&self) -> u64 {
        self.cursor.position
    }

    /// The position of clause `depth`'s variable.
    fn base(&self, depth: usize) -> usize {
        self.scan.clauses[..depth]
            .iter()
            .map(BoundClause::width)
            .sum()
    }

    /// Moves past elements until the next one generated is `index`.
    /// Returns false when the generators end first.
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

    /// Generates the next element, leaving the clauses' variables and
    /// `let`s in the machine for its element, or its key and value, to
    /// read. Returns its index, or `None` once the generators end. A
    /// declared count is never passed, and generators that end before it
    /// are a problem.
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

    /// Computes the `let`s of a cursor resumed from a checkpoint, whose
    /// clauses' values passed their filters when it was kept.
    fn restore<M: Machine<Step = St, Place = P>>(
        &mut self,
        machine: &mut ViewMachine<'_, M>,
    ) -> Result<(), Failure> {
        self.values.clear();
        for depth in 0..self.cursor.levels.len() {
            self.cursor.variables[depth].push(&mut self.values);
            if !self.items(depth, machine)? {
                return Err(internal(
                    "a checkpoint's values no longer pass their filters",
                ));
            }
        }

        self.restored = true;
        Ok(())
    }

    /// Runs clause `depth`'s filters and `let`s on its current value, at
    /// the end of `values`; false when a filter fails.
    fn items<M: Machine<Step = St, Place = P>>(
        &mut self,
        depth: usize,
        machine: &mut ViewMachine<'_, M>,
    ) -> Result<bool, Failure> {
        for item in &self.scan.clauses[depth].items {
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

    /// Advances the generators to their next element, whose variables it
    /// leaves in the machine; false once they end.
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

    /// Starts clause `depth`'s generator, with the values of the clauses
    /// around it in the machine.
    fn enter<M: Machine<Step = St, Place = P>>(
        &mut self,
        depth: usize,
        machine: &mut ViewMachine<'_, M>,
    ) -> Result<(), Failure> {
        let base = self.base(depth);
        self.values.truncate(base);
        machine.set_variables(&self.values);
        let mut run = None;
        let level = match &self.scan.clauses[depth].generator {
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

    /// Clause `depth`'s next value, or `None` when its generator ends. The
    /// values of the clauses around it are in `values`.
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
        let visited = &mut self.visited[depth];

        match (
            &mut self.cursor.levels[depth],
            &self.scan.clauses[depth].generator,
        ) {
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

                // A node seen before, in this run or, by Brent's algorithm,
                // in an earlier one, would be shown again.
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

/// The program, as a kernel's host: its reads and its work are charged to
/// the inspection's budget.
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

/// A kernel's argument, as a 64-bit word: an integer in two's complement,
/// a pointer's address, or a truth value as 0 or 1.
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

/// A node's address: a pointer's value.
fn node<M: Machine>(
    program: &ViewProgram<M::Step>,
    machine: &mut ViewMachine<'_, M>,
) -> Result<u64, Failure> {
    match interp::value(program, machine)? {
        Value::Pointer(address) => Ok(address),
        _ => Err(internal("a node is not a pointer")),
    }
}

/// The node a link leads to from `current`, which the link's program sees
/// at its clause's position, after the values around it.
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
