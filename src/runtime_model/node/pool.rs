//! libuv's thread pool (`deps/uv/src/threadpool.c`), which runs Node's
//! blocking work, as file system calls, `zlib`, and `crypto`'s hashing, on
//! threads of its own. A pool thread with no work waits on the pool's
//! condition variable; one blocked on the pool's mutex is between jobs.
//! Any other runs a job, whose frames are the thread's own. The job keeps
//! no JavaScript callback: Node calls it on the event loop's thread once
//! the job is done, through a request the walk lists apart.

use super::objects::{Problem, Reader};
use super::tasks::pool_job;
use crate::image::v8::UvPool;
use crate::runtime_model::RuntimeTask;
use crate::{TaskState, ThreadId};

/// The most threads libuv's pool runs (`MAX_THREADPOOL_SIZE`).
const MOST_THREADS: u32 = 1024;

/// How far into glibc's `pthread_cond_t` its waiters' futex words lie:
/// `__g_signals`, the two words a waiter blocks on, after `__wseq`,
/// `__g1_start`, and `__g_refs` (`pthreadtypes.h`'s `__pthread_cond_s`).
const COND_WORDS: std::ops::Range<u64> = 0..48;

/// glibc's `pthread_mutex_t` (`__pthread_mutex_s`): its futex word is its
/// first, `__lock`.
const MUTEX_WORDS: std::ops::Range<u64> = 0..40;

/// The pool's threads at a stop, each by its thread, and whether it runs a
/// job.
pub(super) struct Pool {
    pub(super) busy: Vec<ThreadId>,
}

fn u32_at(reader: &Reader<'_>, address: u64) -> Result<u32, Problem> {
    let mut bytes = [0; 4];
    reader.bytes_into(address, &mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

/// The pool's busy threads at this stop, or why they are unknown. A pool
/// libuv has not started has none.
pub(super) fn read(reader: &Reader<'_>, pool: &UvPool) -> Result<Pool, Problem> {
    let stop = reader.stop;
    let bias = stop.load_bias();
    let load = |address| {
        bias.load(address)
            .map(crate::VirtualAddress::get)
            .ok_or_else(|| Problem::from("libuv's pool is outside the address space"))
    };
    let count = u32_at(reader, load(pool.nthreads)?)?;
    if count == 0 {
        return Ok(Pool { busy: Vec::new() });
    }
    if count > MOST_THREADS {
        return Err(format!("libuv's pool says it runs {count} threads").into());
    }
    let threads = reader.word(load(pool.threads)?)?;
    let idle = u32_at(reader, load(pool.idle_threads)?)?;
    let cond = load(pool.cond)?;
    let mutex = load(pool.mutex)?;

    // A pthread_t is the thread's thread pointer.
    let mut members = Vec::new();
    for index in 0..u64::from(count) {
        members.push(reader.word(threads + 8 * index)?);
    }
    let mut found = 0;
    let mut waiting = 0;
    let mut busy = Vec::new();
    for thread in stop.threads() {
        let Some(pointer) = stop.thread_pointer(thread) else {
            continue;
        };
        if !members.contains(&pointer) {
            continue;
        }
        found += 1;
        match stop.blocked_futex(thread) {
            Some(word) if COND_WORDS.contains(&word.wrapping_sub(cond)) => waiting += 1,
            Some(word) if MUTEX_WORDS.contains(&word.wrapping_sub(mutex)) => {}
            _ => busy.push(thread),
        }
    }
    if found != count {
        return Err(format!(
            "libuv's pool runs {count} threads, but {found} of the process's are its"
        )
        .into());
    }
    // A thread counts itself idle before it waits and until it has taken
    // the mutex back, so no more wait than are idle.
    if waiting > idle {
        return Err(format!(
            "{waiting} of libuv's pool threads wait for work, but the pool counts {idle} idle"
        )
        .into());
    }
    Ok(Pool { busy })
}

/// The task of the job the pool thread `thread` runs.
pub(super) fn job(thread: ThreadId) -> RuntimeTask {
    RuntimeTask {
        number: pool_job(thread),
        locator: 0,
        state: TaskState::Running,
        detail: Some("thread-pool job".into()),
        thread: Some(thread),
        resume: None,
        creation: None,
        spawned: None,
        entry: None,
        parent: None,
        internal: false,
        labels: Vec::new(),
        callback: None,
        stackless: false,
    }
}
