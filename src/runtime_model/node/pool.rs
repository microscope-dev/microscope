//! libuv's thread pool (`deps/uv/src/threadpool.c`), which runs Node's
//! blocking work, as file system calls, `zlib`, and `crypto`'s hashing, on
//! threads of its own. A pool thread with no work waits on the pool's
//! condition variable; one blocked on the pool's mutex is between jobs.
//! Any other runs a job, whose frames are the thread's own. The job keeps
//! no JavaScript callback: Node calls it on the event loop's thread once
//! the job is done, through a request the walk lists apart.

use super::objects::{Problem, Reader};
use super::tasks::{pool_job, pool_thread};
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
        members.push(reader.word(threads.wrapping_add(8 * index))?);
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
    debug_assert!(
        busy.len() <= members.len(),
        "{} of libuv's {} pool threads are busy",
        busy.len(),
        members.len()
    );
    Ok(Pool { busy })
}

/// The task of the job the pool thread `thread` runs.
pub(super) fn job(thread: ThreadId) -> RuntimeTask {
    let number = pool_job(thread);
    debug_assert_eq!(
        pool_thread(number),
        Some(thread),
        "the pool job {number:#x} does not name its thread {thread:?}"
    );
    RuntimeTask {
        number,
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::super::objects::Reader;
    use crate::image::v8::{UvPool, V8Version};
    use crate::runtime_model::RuntimeStop;
    use crate::{ImageAddress, LoadBias, ThreadId, VirtualAddress};

    /// Memory the tests write, by address; what no test wrote is
    /// unreadable.
    #[derive(Default)]
    struct Memory(HashMap<u64, u8>);

    impl Memory {
        fn bytes(&mut self, address: u64, bytes: &[u8]) {
            for (at, byte) in (0..).zip(bytes) {
                self.0.insert(address + at, *byte);
            }
        }
    }

    impl RuntimeStop for Memory {
        fn read(&self, address: VirtualAddress, bytes: &mut [u8]) -> bool {
            for (at, byte) in (0..).zip(bytes.iter_mut()) {
                let Some(read) = address.get().checked_add(at).and_then(|at| self.0.get(&at))
                else {
                    return false;
                };
                *byte = *read;
            }
            true
        }

        fn thread_pointer(&self, _thread: ThreadId) -> Option<u64> {
            None
        }

        fn instruction(&self, _thread: ThreadId) -> Option<VirtualAddress> {
            None
        }

        fn load_bias(&self) -> LoadBias {
            LoadBias::default()
        }

        fn threads(&self) -> Vec<ThreadId> {
            Vec::new()
        }
    }

    /// The pool's array of threads is the target's pointer: one near the
    /// top of the address space is unreadable, never an overflow.
    #[test]
    fn a_thread_array_near_the_top_of_the_address_space_is_unreadable() {
        let pool = UvPool {
            threads: ImageAddress::new(0x1000),
            nthreads: ImageAddress::new(0x2000),
            idle_threads: ImageAddress::new(0x3000),
            cond: ImageAddress::new(0x4000),
            mutex: ImageAddress::new(0x5000),
        };
        let mut memory = Memory::default();
        memory.bytes(0x1000, &(u64::MAX - 7).to_le_bytes());
        memory.bytes(0x2000, &2_u32.to_le_bytes());
        memory.bytes(0x3000, &0_u32.to_le_bytes());
        memory.bytes(u64::MAX - 7, &[0; 8]);
        let layout = crate::debug_info::v8::generated_layout(14, 6).expect("a layout");
        let reader = Reader {
            stop: &memory,
            layout: &layout,
            version: V8Version::default(),
            charge: None,
            blocks: None,
        };
        assert!(super::read(&reader, &pool).is_err());
    }
}
