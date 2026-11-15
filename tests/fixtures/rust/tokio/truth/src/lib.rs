










use std::collections::BTreeMap;
use std::fmt::Display;
use std::io::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};


#[derive(Default)]
struct Task {

    awaits: Vec<&'static str>,

    values: BTreeMap<&'static str, String>,

    thread: i32,

    hits: BTreeMap<&'static str, u64>,
}

static TASKS: Mutex<BTreeMap<u64, Task>> = Mutex::new(BTreeMap::new());



static PARKED: AtomicUsize = AtomicUsize::new(0);

fn with_task<T>(id: u64, update: impl FnOnce(&mut Task) -> T) -> T {
    let mut tasks = TASKS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    update(tasks.entry(id).or_default())
}


pub fn id() -> u64 {
    tokio::task::id().to_string().parse().expect("a task id is a number")
}


pub fn gettid() -> i32 {
    // SAFETY: gettid has no preconditions.
    unsafe { libc::gettid() }
}


pub fn start() -> u64 {
    let me = id();
    with_task(me, |task| task.thread = gettid());
    me
}


pub fn end(me: u64) {
    let mut tasks = TASKS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if tasks.remove(&me).is_some_and(|task| !task.awaits.is_empty()) {
        PARKED.fetch_sub(1, Ordering::Relaxed);
    }
}


pub fn here(me: u64) {
    with_task(me, |task| task.thread = gettid());
}


pub fn value(me: u64, name: &'static str, value: impl Display) {
    let text = value.to_string();
    with_task(me, |task| task.values.insert(name, text));
}


pub fn hit(me: u64, tag: &'static str) {
    with_task(me, |task| *task.hits.entry(tag).or_default() += 1);
}


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


pub fn parked_at(tag: &str) -> usize {
    TASKS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .values()
        .filter(|task| task.awaits.last() == Some(&tag))
        .count()
}


pub fn all_parked(expected: usize) -> bool {
    let tasks = TASKS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    tasks.len() == expected && PARKED.load(Ordering::Relaxed) == expected
}



pub fn workers_parked(handle: &tokio::runtime::Handle) -> bool {
    let metrics = handle.metrics();
    (0..metrics.num_workers()).all(|worker| metrics.worker_park_unpark_count(worker) % 2 == 1)
}



pub fn thread_parked(tid: i32) -> bool {
    let task = format!("/proc/self/task/{tid}");
    let sleeping = std::fs::read_to_string(format!("{task}/stat")).is_ok_and(|stat| {

        stat.rsplit_once(") ")
            .is_some_and(|(_, rest)| rest.starts_with('S'))
    });


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



pub fn dump_core_if_asked() {
    if std::env::var_os("TRUTH_CORE").is_some() {
        remove_stack_guards();
        // SAFETY: raising a signal has no preconditions.
        unsafe { libc::raise(libc::SIGTRAP) };
    }
}





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


#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn truth_reached() {
    std::hint::black_box(());
}


#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn task_reached(me: u64) {
    std::hint::black_box(me);
}
