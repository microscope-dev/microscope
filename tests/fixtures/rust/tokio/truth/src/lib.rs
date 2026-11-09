//! What a fixture reports about itself, for tests/tokio to compare the
//! debugger against.
//!
//! Each fixture task calls [`start`] for its own id, which it keeps in a
//! local named `me` saved across its awaits, and [`end`] when its body
//! ends. Before an await a test cares about, it holds the guard [`at`]
//! returns for as long as the await lasts; the await's line carries a
//! `// AWAIT: tag` marker. The nesting of guards is the task's logical
//! stack. At a checkpoint the program waits until nothing moves, prints
//! `TRUTH` lines, and calls [`truth_reached`], where a test stops.

use std::collections::BTreeMap;
use std::fmt::Display;
use std::io::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

/// What one task reported.
#[derive(Default)]
struct Task {
    /// Await tags held, outermost first.
    awaits: Vec<&'static str>,
    /// Values the task recorded, by name.
    values: BTreeMap<&'static str, String>,
    /// The thread the task last reported from.
    thread: i32,
    /// Executions of each counted line.
    hits: BTreeMap<&'static str, u64>,
}

static TASKS: Mutex<BTreeMap<u64, Task>> = Mutex::new(BTreeMap::new());
/// How many of the tasks hold an await, changed only under `TASKS`'s
/// lock: [`all_parked`] counts them without walking every task while the
/// tasks wait for the lock.
static PARKED: AtomicUsize = AtomicUsize::new(0);

fn with_task<T>(id: u64, update: impl FnOnce(&mut Task) -> T) -> T {
    let mut tasks = TASKS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    update(tasks.entry(id).or_default())
}

/// The current task's id, as tokio numbers it.
pub fn id() -> u64 {
    tokio::task::id().to_string().parse().expect("a task id is a number")
}

/// The calling thread's id.
pub fn gettid() -> i32 {
    // SAFETY: gettid has no preconditions.
    unsafe { libc::gettid() }
}

/// Registers the current task as alive and returns its id.
pub fn start() -> u64 {
    let me = id();
    with_task(me, |task| task.thread = gettid());
    me
}

/// Removes a task whose body has ended.
pub fn end(me: u64) {
    let mut tasks = TASKS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if tasks.remove(&me).is_some_and(|task| !task.awaits.is_empty()) {
        PARKED.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Records that the task is on this thread now.
pub fn here(me: u64) {
    with_task(me, |task| task.thread = gettid());
}

/// Records a value the task's checkpoint names.
pub fn value(me: u64, name: &'static str, value: impl Display) {
    let text = value.to_string();
    with_task(me, |task| task.values.insert(name, text));
}

/// Counts one execution of a line.
pub fn hit(me: u64, tag: &'static str) {
    with_task(me, |task| *task.hits.entry(tag).or_default() += 1);
}

/// A task's await, held for as long as the await lasts.
pub struct At {
    me: u64,
}

impl Drop for At {
    fn drop(&mut self) {
        with_task(self.me, |task| {
            task.awaits.pop();
            if task.awaits.is_empty() {
                PARKED.fetch_sub(1, Ordering::Relaxed);
            }
            task.thread = gettid();
        });
    }
}

/// Marks the await the task is about to make.
#[must_use]
pub fn at(me: u64, tag: &'static str) -> At {
    with_task(me, |task| {
        if task.awaits.is_empty() {
            PARKED.fetch_add(1, Ordering::Relaxed);
        }
        task.awaits.push(tag);
    });
    At { me }
}

/// Prints one `TRUTH` line.
pub fn line(fields: &[&dyn Display]) {
    let mut text = String::from("TRUTH");
    for field in fields {
        text.push('\t');
        text.push_str(&field.to_string());
    }
    text.push('\n');
    let mut out = std::io::stdout().lock();
    out.write_all(text.as_bytes()).expect("print a truth line");
    out.flush().expect("flush a truth line");
}

/// The number of tasks that registered an await `tag`.
pub fn parked_at(tag: &str) -> usize {
    TASKS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .values()
        .filter(|task| task.awaits.last() == Some(&tag))
        .count()
}

/// Whether every task registered has an await registered.
pub fn all_parked(expected: usize) -> bool {
    let tasks = TASKS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    tasks.len() == expected && PARKED.load(Ordering::Relaxed) == expected
}

/// Whether every worker of the runtime is parked: an odd park count
/// means a worker is parked, tokio says.
pub fn workers_parked(handle: &tokio::runtime::Handle) -> bool {
    let metrics = handle.metrics();
    (0..metrics.num_workers()).all(|worker| metrics.worker_park_unpark_count(worker) % 2 == 1)
}

/// Whether the thread `tid` sleeps in a futex wait, as a thread parked on
/// a condition variable does.
pub fn thread_parked(tid: i32) -> bool {
    let task = format!("/proc/self/task/{tid}");
    let sleeping = std::fs::read_to_string(format!("{task}/stat")).is_ok_and(|stat| {
        // The state follows the command's closing parenthesis.
        stat.rsplit_once(") ")
            .is_some_and(|(_, rest)| rest.starts_with('S'))
    });
    // A timed wait that a debugger's stop interrupted goes on in
    // `restart_syscall`, still in the futex.
    let in_futex = std::fs::read_to_string(format!("{task}/syscall")).is_ok_and(|syscall| {
        match syscall.split_whitespace().next() {
            Some("202") => true,
            Some("219") => std::fs::read_to_string(format!("{task}/wchan"))
                .is_ok_and(|wchan| wchan.starts_with("futex")),
            _ => false,
        }
    });
    sleeping && in_futex
}

/// Prints a checkpoint's lines and stops there under a debugger. The
/// caller has waited until nothing moves. With `TRUTH_CORE` set, the
/// program then traps, for the debugger that runs it to dump its core.
pub fn checkpoint(name: &str, handle: Option<&tokio::runtime::Handle>) {
    line(&[&"checkpoint", &name]);
    if let Some(handle) = handle {
        let metrics = handle.metrics();
        line(&[&"alive", &metrics.num_alive_tasks()]);
        line(&[&"workers", &metrics.num_workers()]);
    }
    {
        let tasks = TASKS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        for (id, task) in tasks.iter() {
            let awaits = task.awaits.iter().rev().copied().collect::<Vec<_>>().join(",");
            line(&[&"task", id, &awaits, &task.thread]);
            for (name, value) in &task.values {
                line(&[&"value", id, name, value]);
            }
            for (tag, count) in &task.hits {
                line(&[&"hit", id, tag, count]);
            }
        }
    }
    line(&[&"main", &gettid()]);
    truth_reached();
    dump_core_if_asked();
}

/// With `TRUTH_CORE` set, traps, for the debugger that runs the program to
/// dump its core.
pub fn dump_core_if_asked() {
    if std::env::var_os("TRUTH_CORE").is_some() {
        remove_stack_guards();
        // SAFETY: raising a signal has no preconditions.
        unsafe { libc::raise(libc::SIGTRAP) };
    }
}

/// Takes the guards out of every writable mapping. glibc 2.42 guards each
/// thread's stack with `MADV_GUARD_INSTALL` inside the stack's mapping,
/// which gdb's `gcore` cannot read, and so saves the whole stack as zeros;
/// a mapping with no guard is left as it was.
fn remove_stack_guards() {
    const MADV_GUARD_REMOVE: libc::c_int = 103;
    let maps = std::fs::read_to_string("/proc/self/maps").expect("the program's mappings");
    for mapping in maps.lines() {
        let mut fields = mapping.split_whitespace();
        let (Some(range), Some(permissions)) = (fields.next(), fields.next()) else {
            continue;
        };
        let Some((start, end)) = range.split_once('-') else {
            continue;
        };
        let (Ok(start), Ok(end)) = (
            usize::from_str_radix(start, 16),
            usize::from_str_radix(end, 16),
        ) else {
            continue;
        };
        if permissions.starts_with("rw") {
            // SAFETY: removing guards changes no mapped memory.
            unsafe { libc::madvise(start as *mut libc::c_void, end - start, MADV_GUARD_REMOVE) };
        }
    }
}

/// Where a debugger stops at each checkpoint.
#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn truth_reached() {
    std::hint::black_box(());
}

/// Where a debugger stops in a task, which passes its own id.
#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn task_reached(me: u64) {
    std::hint::black_box(me);
}
