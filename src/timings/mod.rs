//! Where microscope's time and memory go.
//!
//! Code marks its phases with [`span!`] and counts its work with
//! [`count!`]; a [`Recording`] collects both from every thread into a
//! [`Report`], which `--timings` writes as JSON with Chrome trace events
//! and `microscope-tools timings` summarizes as text.
//!
//! Nothing is measured unless a recording is running: a span then costs one
//! predictable branch, and reads no clock and allocates nothing. Spans mark
//! phases and units, never single entries or rows; hot loops count instead,
//! and their counts attach to the innermost open span on their thread.

pub mod alloc;
mod report;

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Instant;

pub use report::{Phase, Report, Summary, ThreadBusy, TraceEvent};

/// The most span events one recording keeps; later ones are counted as
/// dropped.
const MAX_EVENTS: usize = 1 << 18;

/// The longest dynamic label a span keeps, in bytes.
const MAX_LABEL: usize = 96;

/// Whether a recording is running. Spans check this alone when none is.
static ENABLED: AtomicBool = AtomicBool::new(false);

/// Which recording is running, so a span begun under one is not reported
/// by the next.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// The next thread's number in reports.
static NEXT_THREAD: AtomicU32 = AtomicU32::new(0);

/// The next span's identifier, which a span on another thread names as its
/// parent.
static NEXT_SPAN: AtomicU64 = AtomicU64::new(1);

/// The running recording's collected events.
static COLLECTED: Mutex<Option<Collected>> = Mutex::new(None);

struct Collected {
    generation: u64,
    epoch: Instant,
    events: Vec<Event>,
    marks: Vec<(&'static str, u64)>,

    /// Counts made outside any span.
    loose: Vec<(&'static str, u64)>,
    dropped: u64,
    instructions: bool,
}

/// One finished span.
#[derive(Debug, Clone)]
pub(crate) struct Event {
    pub name: &'static str,
    pub label: Option<Box<str>>,
    pub id: u64,

    /// The span that started this one's work, when it ran on another thread.
    pub parent: Option<u64>,
    pub thread: u32,
    pub depth: u32,
    pub start_ns: u64,
    pub wall_ns: u64,
    pub cpu_ns: u64,
    pub instructions: Option<u64>,
    pub allocations: alloc::Allocated,
    pub counters: Vec<(&'static str, u64)>,
}

struct Open {
    name: &'static str,
    label: Option<Box<str>>,
    id: u64,
    parent: Option<u64>,
    started: Instant,
    cpu: u64,
    instructions: Option<u64>,
    allocations: alloc::Allocated,
    counters: Vec<(&'static str, u64)>,
}

struct ThreadState {
    number: u32,
    generation: u64,

    /// The running recording's start, and whether it counts instructions.
    epoch: Instant,
    instructions_wanted: bool,
    open: Vec<Open>,
    finished: Vec<Event>,
    instructions: Option<Instructions>,
}

thread_local! {
    static THREAD: RefCell<Option<ThreadState>> = const { RefCell::new(None) };
}

/// A thread's retired user-mode instructions, counted for this thread
/// alone: the counter follows neither other threads nor child processes,
/// so a debuggee never inherits it.
struct Instructions {
    counter: Option<perf_event::Counter>,
}

impl Instructions {
    fn open() -> Self {
        let counter = perf_event::Builder::new()
            .kind(perf_event::events::Hardware::INSTRUCTIONS)
            .build()
            .and_then(|mut counter| counter.enable().map(|()| counter))
            .inspect_err(|error| note_instructions_unavailable(&error.to_string()))
            .ok();
        Self { counter }
    }

    fn read(&mut self) -> Option<u64> {
        let counter = self.counter.as_mut()?;

        match counter.read_count_and_time() {
            Ok(read) => {
                if read.time_running < read.time_enabled {
                    MULTIPLEXED.store(true, Ordering::Relaxed);
                }

                Some(read.count)
            }
            Err(error) => {
                note_instructions_unavailable(&error.to_string());
                self.counter = None;
                None
            }
        }
    }
}

static MULTIPLEXED: AtomicBool = AtomicBool::new(false);
static INSTRUCTIONS_UNAVAILABLE: Mutex<Option<String>> = Mutex::new(None);

fn note_instructions_unavailable(reason: &str) {
    let mut unavailable = INSTRUCTIONS_UNAVAILABLE
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    unavailable.get_or_insert_with(|| reason.to_owned());
}

/// What a recording measures beyond time and allocations.
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// Count each span's retired user-mode instructions with a hardware
    /// counter, where the kernel allows one.
    pub instructions: bool,
}

/// Why a recording could not start: another is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a profile recording is already running")]
pub struct AlreadyRecording;

/// A running recording. Only one runs at a time in a process.
#[must_use = "a recording collects nothing once dropped"]
pub struct Recording {
    generation: u64,
    started: Instant,
    cpu: u64,
    allocations: alloc::Totals,
}

impl Recording {
    /// Starts recording every thread's spans and counts, or fails when a
    /// recording is already running.
    pub fn start(options: Options) -> Result<Self, AlreadyRecording> {
        let mut collected = COLLECTED.lock().unwrap_or_else(PoisonError::into_inner);
        if collected.is_some() {
            drop(collected);
            return Err(AlreadyRecording);
        }

        let generation = GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
        MULTIPLEXED.store(false, Ordering::Relaxed);
        *INSTRUCTIONS_UNAVAILABLE
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
        let started = Instant::now();

        *collected = Some(Collected {
            generation,
            epoch: started,
            events: Vec::new(),
            marks: Vec::new(),
            loose: Vec::new(),
            dropped: 0,
            instructions: options.instructions,
        });
        drop(collected);
        alloc::start_totals();
        ENABLED.store(true, Ordering::Release);
        Ok(Self {
            generation,
            started,
            cpu: process_cpu_ns(),
            allocations: alloc::totals(),
        })
    }

    /// Stops recording and reports what it collected.
    pub fn finish(self) -> crate::Result<Report> {
        ENABLED.store(false, Ordering::Release);
        let wall_ns = elapsed_ns(self.started);
        let cpu_ns = process_cpu_ns().saturating_sub(self.cpu);
        let allocations = alloc::totals().since(&self.allocations);
        alloc::stop_totals();

        // This thread's spans may still be buffered.
        flush_thread();
        let collected = COLLECTED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .filter(|collected| collected.generation == self.generation)
            .ok_or_else(|| crate::Error::internal("the recording's collection is another's"))?;
        let unavailable = INSTRUCTIONS_UNAVAILABLE
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let instructions = if !collected.instructions {
            report::CounterStatus::Off
        } else if let Some(reason) = unavailable {
            report::CounterStatus::Unavailable(reason)
        } else if MULTIPLEXED.load(Ordering::Relaxed) {
            report::CounterStatus::Multiplexed
        } else {
            report::CounterStatus::Counted
        };

        Ok(Report::new(report::Raw {
            wall_ns,
            cpu_ns,
            peak_rss_bytes: peak_rss_bytes(),
            allocations,
            events: collected.events,
            marks: collected.marks,
            loose: collected.loose,
            dropped: collected.dropped,
            instructions,
        }))
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        let mut collected = COLLECTED.lock().unwrap_or_else(PoisonError::into_inner);
        if collected
            .as_ref()
            .is_some_and(|collected| collected.generation == self.generation)
        {
            ENABLED.store(false, Ordering::Release);
            alloc::stop_totals();
            *collected = None;
        }
    }
}

/// Whether a recording is running.
#[inline]
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Marks an instant, such as the moment a session is ready for its first
/// command.
pub fn mark(name: &'static str) {
    if !enabled() {
        return;
    }

    let mut collected = COLLECTED.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(collected) = collected.as_mut() {
        let at = elapsed_ns(collected.epoch);
        collected.marks.push((name, at));
    }
}

/// A span open on this thread until dropped.
#[must_use = "a span measures until it is dropped"]
pub struct Span {
    open: bool,
}

impl Span {
    /// The identifier work on another thread names as its parent, or `None`
    /// when nothing is recording.
    #[must_use]
    pub fn id(&self) -> Option<u64> {
        if !self.open {
            return None;
        }

        THREAD.with(|thread| {
            thread
                .borrow()
                .as_ref()
                .and_then(|state| state.open.last().map(|open| open.id))
        })
    }
}

/// Opens a span named `name`, with a label `label` formats only when a
/// recording is running. Work that runs on another thread for this span
/// names it as `parent`.
#[inline]
pub fn span(name: &'static str, parent: Option<u64>, label: Option<&dyn Fn() -> String>) -> Span {
    Span {
        open: enabled() && open_span(name, parent, label),
    }
}

/// Opens a span in the running recording, unless it has just ended.
#[cold]
fn open_span(name: &'static str, parent: Option<u64>, label: Option<&dyn Fn() -> String>) -> bool {
    let label = label.map(|label| {
        let mut text = label();
        if text.len() > MAX_LABEL {
            let mut end = MAX_LABEL;
            while !text.is_char_boundary(end) {
                end -= 1;
            }

            text.truncate(end);
        }

        text.into_boxed_str()
    });
    THREAD.with(|thread| {
        let mut thread = thread.borrow_mut();
        let generation = GENERATION.load(Ordering::Relaxed);

        let state = match thread.as_mut() {
            Some(state) if state.generation == generation => state,
            _ => {
                // The recording this thread last saw has ended.
                let Some((current, epoch, instructions_wanted)) = COLLECTED
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .as_ref()
                    .map(|collected| {
                        (
                            collected.generation,
                            collected.epoch,
                            collected.instructions,
                        )
                    })
                else {
                    return false;
                };

                let number = thread.as_ref().map_or_else(
                    || NEXT_THREAD.fetch_add(1, Ordering::Relaxed),
                    |state| state.number,
                );
                let instructions = thread.take().and_then(|state| state.instructions);
                thread.insert(ThreadState {
                    number,
                    generation: current,
                    epoch,
                    instructions_wanted,
                    open: Vec::new(),
                    finished: Vec::new(),
                    instructions,
                })
            }
        };

        if state.instructions_wanted && state.instructions.is_none() {
            state.instructions = Some(Instructions::open());
        }

        let instructions = if state.instructions_wanted {
            state.instructions.as_mut().and_then(Instructions::read)
        } else {
            None
        };

        state.open.push(Open {
            name,
            label,
            id: NEXT_SPAN.fetch_add(1, Ordering::Relaxed),
            parent,
            started: Instant::now(),
            cpu: thread_cpu_ns(),
            instructions,
            allocations: alloc::thread_allocated(),
            counters: Vec::new(),
        });
        true
    })
}

impl Drop for Span {
    #[inline]
    fn drop(&mut self) {
        if self.open {
            close_span();
        }
    }
}

#[cold]
fn close_span() {
    let generation = GENERATION.load(Ordering::Relaxed);
    let outermost = THREAD.with(|thread| {
        let mut thread = thread.borrow_mut();
        let Some(state) = thread.as_mut() else {
            return false;
        };

        let Some(open) = state.open.pop() else {
            return false;
        };

        if state.generation != generation || !enabled() {
            return state.open.is_empty();
        }

        let wall_ns = elapsed_ns(open.started);
        let cpu_ns = thread_cpu_ns().saturating_sub(open.cpu);
        let instructions = match (open.instructions, state.instructions.as_mut()) {
            (Some(before), Some(counter)) => counter.read().map(|now| now.saturating_sub(before)),
            _ => None,
        };

        let allocations = alloc::thread_allocated().since(&open.allocations);
        let epoch_offset = u64::try_from(
            open.started
                .saturating_duration_since(state.epoch)
                .as_nanos(),
        )
        .unwrap_or(u64::MAX);

        state.finished.push(Event {
            name: open.name,
            label: open.label,
            id: open.id,
            parent: open.parent,
            thread: state.number,
            depth: u32::try_from(state.open.len()).unwrap_or(u32::MAX),
            start_ns: epoch_offset,
            wall_ns,
            cpu_ns,
            instructions,
            allocations,
            counters: open.counters,
        });

        state.open.is_empty()
    });
    if outermost {
        flush_thread();
    }
}

/// Moves this thread's finished spans into the recording they were
/// finished under, unless it has ended.
fn flush_thread() {
    let Some((generation, finished)) = THREAD.with(|thread| {
        thread
            .borrow_mut()
            .as_mut()
            .map(|state| (state.generation, std::mem::take(&mut state.finished)))
    }) else {
        return;
    };

    if finished.is_empty() {
        return;
    }

    if let Some(collected) = COLLECTED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_mut()
        .filter(|collected| collected.generation == generation)
    {
        for event in finished {
            if collected.events.len() < MAX_EVENTS {
                collected.events.push(event);
            } else {
                collected.dropped += 1;
            }
        }
    }
}

/// Adds `amount` to the counter `name` of the innermost span open on this
/// thread.
#[inline]
pub fn count(name: &'static str, amount: u64) {
    if enabled() {
        add_count(name, amount);
    }
}

#[cold]
fn add_count(name: &'static str, amount: u64) {
    let counted = THREAD.with(|thread| {
        let mut thread = thread.borrow_mut();
        let Some(open) = thread.as_mut().and_then(|state| state.open.last_mut()) else {
            return false;
        };

        add_to(&mut open.counters, name, amount);
        true
    });
    if !counted {
        let mut collected = COLLECTED.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(collected) = collected.as_mut() {
            add_to(&mut collected.loose, name, amount);
        }
    }
}

fn add_to(counters: &mut Vec<(&'static str, u64)>, name: &'static str, amount: u64) {
    match counters.iter_mut().find(|(counter, _)| *counter == name) {
        Some((_, total)) => *total = total.saturating_add(amount),
        None => counters.push((name, amount)),
    }
}

/// Opens a span until the end of the enclosing block: `span!("lines")`,
/// with a label formatted only while recording, `span!("unit", "{offset:#x}")`,
/// or as part of work another span started, `span!(parent = id, "unit")`.
#[macro_export]
#[doc(hidden)]
macro_rules! span {
    (parent = $parent:expr, $name:literal) => {
        $crate::timings::span($name, $parent, None)
    };
    (parent = $parent:expr, $name:literal, $($label:tt)+) => {
        $crate::timings::span($name, $parent, Some(&|| ::std::format!($($label)+)))
    };
    ($name:literal) => {
        $crate::timings::span($name, None, None)
    };
    ($name:literal, $($label:tt)+) => {
        $crate::timings::span($name, None, Some(&|| ::std::format!($($label)+)))
    };
}

/// Adds to a counter of the innermost open span: `count!("dies", n)`.
#[macro_export]
#[doc(hidden)]
macro_rules! count {
    ($name:literal, $amount:expr) => {
        $crate::timings::count(
            $name,
            ::core::convert::TryInto::try_into($amount).unwrap_or(u64::MAX),
        )
    };
}

fn elapsed_ns(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

fn clock_ns(clock: nix::time::ClockId) -> u64 {
    nix::time::clock_gettime(clock).map_or(0, |time| {
        u64::try_from(time.tv_sec())
            .unwrap_or(0)
            .saturating_mul(1_000_000_000)
            .saturating_add(u64::try_from(time.tv_nsec()).unwrap_or(0))
    })
}

/// The CPU time this thread has used.
fn thread_cpu_ns() -> u64 {
    clock_ns(nix::time::ClockId::CLOCK_THREAD_CPUTIME_ID)
}

/// The CPU time every thread of the process has used.
fn process_cpu_ns() -> u64 {
    clock_ns(nix::time::ClockId::CLOCK_PROCESS_CPUTIME_ID)
}

/// The process's peak resident set: a high-water mark for its whole life,
/// not something a span can subtract.
fn peak_rss_bytes() -> Option<u64> {
    let usage = nix::sys::resource::getrusage(nix::sys::resource::UsageWho::RUSAGE_SELF).ok()?;

    u64::try_from(usage.max_rss())
        .ok()
        .map(|kib| kib.saturating_mul(1024))
}

#[cfg(test)]
mod tests;
