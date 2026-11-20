pub mod decode;
pub mod jit;
mod lease;
pub mod sys;

use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::path::PathBuf;
use std::process::{Command, ExitStatus};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use nix::libc;

use super::modules::{ModuleMapping, identify_mapped_module};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Clock { period: Duration },

    Breakpoint { address: u64, period: u64 },

    Mappings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventOptions {
    pub source: Source,

    pub stack_bytes: u32,

    pub kernel: bool,

    pub ring_pages: usize,

    pub switches: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error(
        "the kernel does not let this user sample the program (perf_event_paranoid is {paranoid}): {error}; kernel sampling needs CAP_PERFMON (or CAP_SYS_ADMIN), or a lower kernel.perf_event_paranoid setting"
    )]
    Denied {
        paranoid: String,
        error: std::io::Error,
    },

    #[error("opening a perf event on CPU {cpu} failed: {error}")]
    Event { cpu: u32, error: std::io::Error },

    #[error("mapping a perf ring failed: {0}")]
    Ring(std::io::Error),

    #[error(
        "this user's memory for perf rings is in use: {pages} pages per CPU, the fewest that hold two samples, do not fit"
    )]
    RingBudget { pages: usize },

    #[error("creating the perf reader's wakeup failed: {0}")]
    Handoff(std::io::Error),

    #[error("reading the online CPUs failed: {0}")]
    Cpus(std::io::Error),

    #[error("the process cannot be read: {0}")]
    NoProcess(std::io::Error),

    #[error(
        "sampling {threads} threads on {cpus} CPUs takes {events} events, more than the budget of {budget}"
    )]
    TooManyEvents {
        threads: usize,
        cpus: usize,
        events: usize,
        budget: usize,
    },
}

#[derive(Debug)]
pub struct Events {
    rings: Vec<sys::Ring>,
    pages: usize,

    buffered: Option<Mutex<Queue>>,

    pause_on_full: bool,

    queue_bytes: usize,

    shortened: AtomicU64,

    reader_wait: Duration,
    reader_cpu: AtomicU64,
    handed: OwnedFd,

    cpus: Vec<u32>,

    followers: Vec<OwnedFd>,

    sideband: Vec<OwnedFd>,

    gate: Mutex<Gate>,

    _lease: lease::Lease,
}

const QUEUE_BYTES: usize = 64 * 1024 * 1024;

const SHORT_STACK_BYTES: usize = 512;

const BLOCK_BYTES: usize = 4 * 1024 * 1024;

const FREE_BLOCKS: usize = QUEUE_BYTES / BLOCK_BYTES;

#[derive(Debug, Default)]
pub struct Blocks {
    pub filled: Vec<Vec<u8>>,
    pub emptied: Vec<Vec<u8>>,
}

#[derive(Debug, Default)]
struct Queue {
    blocks: Vec<Vec<u8>>,

    bytes: usize,

    free: Vec<Vec<u8>>,
}

impl Queue {
    #[expect(
        clippy::indexing_slicing,
        reason = "a block is pushed when there is none, so the last exists"
    )]
    fn room(&mut self, room: usize, block: usize) -> &mut Vec<u8> {
        debug_assert!(room <= block, "{room} bytes do not fit a block of {block}");
        if self
            .blocks
            .last()
            .is_none_or(|last| last.capacity() - last.len() < room)
        {
            let mut next = self.free.pop().unwrap_or_default();
            next.clear();
            next.reserve(block);
            self.blocks.push(next);
        }

        let last = self.blocks.len() - 1;
        &mut self.blocks[last]
    }

    fn drain_ring(&mut self, ring: &sys::Ring, room: usize, block: usize) {
        let into = self.room(room, block);
        let (at, capacity) = (into.as_ptr(), into.capacity());
        let drained = ring.drain(into);
        debug_assert!(
            into.as_ptr() == at && into.capacity() == capacity,
            "a block grew, so its records were copied again"
        );
        self.bytes += drained;
    }

    fn take_back(&mut self, emptied: &mut Vec<Vec<u8>>) {
        for mut block in emptied.drain(..) {
            if self.free.len() < FREE_BLOCKS {
                block.clear();
                self.free.push(block);
            }
        }
    }

    fn hand_on(&mut self, filled: &mut Vec<Vec<u8>>) -> usize {
        for block in self.blocks.drain(..) {
            if block.is_empty() {
                self.free.push(block);
            } else {
                filled.push(block);
            }
        }
        std::mem::take(&mut self.bytes)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hold {
    Snapshot,

    Behind,

    Ended,
}

#[derive(Debug, Default)]
struct Gate {
    held: u8,

    behind: Option<u64>,
    spans: Vec<(u64, u64)>,
}

impl Gate {
    fn hold(&mut self, who: Hold, held: bool, now: u64) -> Option<bool> {
        debug_assert_ne!(self.holds(who), held, "{who:?} held or let go twice");
        debug_assert!(
            held || who != Hold::Ended,
            "sampling resumed after it ended"
        );

        let was = self.held == 0;
        self.held ^= 1 << who as u8;
        if who == Hold::Behind {
            if held {
                self.behind = Some(now);
            } else if let Some(since) = self.behind.take() {
                self.spans.push((since, now));
            }
        }

        let on = self.held == 0;
        (on != was).then_some(on)
    }

    const fn holds(&self, who: Hold) -> bool {
        self.held & 1 << who as u8 != 0
    }

    fn take_spans(&mut self, until: Option<u64>) -> Vec<(u64, u64)> {
        if let Some(until) = until
            && let Some(since) = self.behind.take()
        {
            self.spans.push((since, until.max(since)));
        }

        std::mem::take(&mut self.spans)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pass {
    Wait,

    Drain { behind: bool, shorten: bool },
}

impl Pass {
    const fn of(queued: usize, limit: usize, pause_on_full: bool, shortens: bool) -> Self {
        let full = queued >= limit;
        if full && !pause_on_full {
            return Self::Wait;
        }

        Self::Drain {
            behind: full,
            shorten: shortens && queued >= limit / 4 * 3,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub counted: u64,

    pub lost_samples: u64,

    pub lost_records: u64,

    pub untrusted: u64,

    pub shortened: u64,
}

pub const OUTPUT_SAMPLE_TYPE: u64 = sys::sample::IDENTIFIER
    | sys::sample::TID
    | sys::sample::TIME
    | sys::sample::CPU
    | sys::sample::RAW;

#[derive(Debug)]
pub struct Output {
    pub cpu: u32,
    pub fd: OwnedFd,
    pub id: u64,
}

const fn max_sample_bytes(stack_bytes: u32) -> usize {
    const FIXED: usize = 8 * 8;
    const CHAIN: usize = 8 * (1 + 127 + 8);
    const REGISTERS: usize = 8 * (1 + decode::REGISTERS.count_ones() as usize);
    FIXED + CHAIN + REGISTERS + 16 + stack_bytes as usize
}

fn min_pages(stack_bytes: u32) -> usize {
    (2 * max_sample_bytes(stack_bytes))
        .div_ceil(sys::page_size())
        .next_power_of_two()
}

impl Events {
    pub fn open(pid: u32, options: &EventOptions) -> Result<Self, OpenError> {
        let cpus = online_cpus().map_err(OpenError::Cpus)?;
        let leased = Leased::take(options, cpus.len());
        Self::open_leased(pid, options, leased)
    }

    fn open_leased(pid: u32, options: &EventOptions, leased: Leased) -> Result<Self, OpenError> {
        let start = sys::flag::DISABLED | sys::flag::ENABLE_ON_EXEC;
        let pid = i32::try_from(pid).map_err(|_| OpenError::Cpus(std::io::Error::other("pid")))?;
        let cpus = online_cpus().map_err(OpenError::Cpus)?;
        let (rings, pages, _, lease) = rings(attr(options, start), pid, &cpus, options, leased)?;
        let sideband = match sideband_attr(options, start) {
            Some(attr) => into_rings(&attr, pid, &cpus, &rings)?,
            None => Vec::new(),
        };
        record!(
            "perf events for {pid}: {} CPUs, {pages} pages each",
            rings.len()
        );

        Ok(Self {
            rings,
            pages,
            buffered: collection_buffer(options),
            pause_on_full: matches!(options.source, Source::Clock { .. }),
            queue_bytes: QUEUE_BYTES,
            shortened: AtomicU64::new(0),
            reader_wait: reader_wait(options),
            reader_cpu: AtomicU64::new(0),
            handed: sys::handoff().map_err(OpenError::Handoff)?,
            cpus,
            followers: Vec::new(),
            sideband,
            gate: Mutex::default(),
            _lease: lease,
        })
    }

    pub fn attach(pid: u32, options: &EventOptions, budget: usize) -> Result<Self, OpenError> {
        let cpus = online_cpus().map_err(OpenError::Cpus)?;
        Self::attach_on(pid, options, budget, &cpus)
    }

    fn attach_held(pid: u32, options: &EventOptions, budget: usize) -> Result<Self, OpenError> {
        let cpus = online_cpus().map_err(OpenError::Cpus)?;
        Self::attach_starting(pid, options, budget, &cpus, sys::flag::DISABLED)
    }

    pub fn attach_on(
        pid: u32,
        options: &EventOptions,
        budget: usize,
        cpus: &[u32],
    ) -> Result<Self, OpenError> {
        Self::attach_starting(pid, options, budget, cpus, 0)
    }

    fn attach_starting(
        pid: u32,
        options: &EventOptions,
        budget: usize,
        cpus: &[u32],
        start: u64,
    ) -> Result<Self, OpenError> {
        raise_descriptor_limit();
        let mut attr = attr(options, start);
        let sideband = sideband_attr(options, 0);
        let per_thread = cpus.len() * if sideband.is_some() { 2 } else { 1 };
        let mut opened = std::collections::BTreeSet::new();
        let mut owner = None;

        loop {
            let mut threads = threads(pid).map_err(OpenError::NoProcess)?;
            for child in descendants(pid) {
                threads.extend(self::threads(child).unwrap_or_default());
            }
            let new: Vec<u32> = threads
                .into_iter()
                .filter(|tid| !opened.contains(tid))
                .collect();
            if new.is_empty() {
                break;
            }

            let threads = opened.len() + new.len();
            if threads * per_thread > budget {
                return Err(OpenError::TooManyEvents {
                    threads,
                    cpus: cpus.len(),
                    events: threads * per_thread,
                    budget,
                });
            }

            for tid in new {
                opened.insert(tid);
                let thread = i32::try_from(tid).unwrap_or(i32::MAX);

                let Some(events) = owner.as_mut() else {
                    let leased = Leased::take(options, cpus.len());
                    match rings(attr, thread, cpus, options, leased) {
                        Ok((rings, pages, watermark, lease)) => {
                            attr.wakeup_watermark = watermark;
                            let sideband = match &sideband {
                                Some(sideband) => into_rings(sideband, thread, cpus, &rings)?,
                                None => Vec::new(),
                            };
                            owner = Some(Self {
                                rings,
                                pages,
                                buffered: collection_buffer(options),
                                pause_on_full: matches!(options.source, Source::Clock { .. }),
                                queue_bytes: QUEUE_BYTES,
                                shortened: AtomicU64::new(0),
                                reader_wait: reader_wait(options),
                                reader_cpu: AtomicU64::new(0),
                                handed: sys::handoff().map_err(OpenError::Handoff)?,
                                cpus: cpus.to_vec(),
                                followers: Vec::new(),
                                sideband,
                                gate: Mutex::default(),
                                _lease: lease,
                            });
                        }

                        Err(OpenError::Event { error, .. })
                            if error.raw_os_error() == Some(libc::ESRCH) => {}
                        Err(error) => return Err(error),
                    }

                    continue;
                };

                events.follow(&attr, sideband.as_ref(), thread, cpus)?;
            }
        }

        let events = owner
            .ok_or_else(|| OpenError::NoProcess(std::io::Error::from_raw_os_error(libc::ESRCH)))?;
        record!(
            "perf attach to {pid}: {} threads, {} CPUs, {} pages each",
            opened.len(),
            cpus.len(),
            events.pages
        );

        Ok(events)
    }

    fn follow(
        &mut self,
        attr: &sys::Attr,
        sideband: Option<&sys::Attr>,
        thread: i32,
        cpus: &[u32],
    ) -> Result<(), OpenError> {
        for (ring, &cpu) in self.rings.iter().zip(cpus) {
            for (attr, fds) in [
                (Some(attr), &mut self.followers),
                (sideband, &mut self.sideband),
            ] {
                let Some(attr) = attr else {
                    continue;
                };
                let fd = match sys::open(attr, thread, i32::try_from(cpu).unwrap_or(i32::MAX)) {
                    Ok(fd) => fd,
                    Err(error) if error.raw_os_error() == Some(libc::ESRCH) => return Ok(()),
                    Err(error) => return Err(OpenError::Event { cpu, error }),
                };

                sys::set_output(&fd, ring.fd()).map_err(|error| OpenError::Event { cpu, error })?;
                fds.push(fd);
            }
        }

        Ok(())
    }

    pub const fn pages(&self) -> usize {
        self.pages
    }

    pub fn run_program(&self, program: std::os::fd::BorrowedFd<'_>) -> std::io::Result<()> {
        for fd in self.rings.iter().map(sys::Ring::fd).chain(&self.followers) {
            sys::set_bpf(fd, program)?;
        }

        Ok(())
    }

    pub fn outputs(&self) -> Result<Vec<Output>, OpenError> {
        let attr = sys::Attr {
            size: sys::ATTR_SIZE,
            kind: sys::TYPE_SOFTWARE,
            config: sys::COUNT_SW_BPF_OUTPUT,
            sample_period: 1,
            sample_type: OUTPUT_SAMPLE_TYPE,
            flags: sys::flag::USE_CLOCKID,
            clockid: libc::CLOCK_MONOTONIC,
            ..sys::Attr::default()
        };

        self.rings
            .iter()
            .zip(&self.cpus)
            .map(|(ring, &cpu)| {
                let failed = |error| OpenError::Event { cpu, error };
                let fd =
                    sys::open(&attr, -1, i32::try_from(cpu).unwrap_or(i32::MAX)).map_err(failed)?;
                sys::set_output(&fd, ring.fd()).map_err(failed)?;
                let id = sys::id(&fd).map_err(failed)?;

                Ok(Output { cpu, fd, id })
            })
            .collect()
    }

    pub fn drain(&self, out: &mut Vec<u8>) -> usize {
        if self.buffered.is_none() {
            let before = out.len();
            for ring in &self.rings {
                ring.drain(out);
            }
            return out.len() - before;
        }

        let mut blocks = Blocks::default();
        let drained = self.drain_blocks(&mut blocks);
        for block in &blocks.filled {
            out.extend_from_slice(block);
        }
        blocks.emptied.append(&mut blocks.filled);
        if let Some(queue) = &self.buffered {
            queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take_back(&mut blocks.emptied);
        }
        drained
    }

    pub fn drain_blocks(&self, blocks: &mut Blocks) -> usize {
        let (block, ring_bytes) = (self.block_bytes(), self.pages * sys::page_size());
        let Some(queue) = &self.buffered else {
            let mut queue = Queue::default();
            queue.take_back(&mut blocks.emptied);
            for ring in &self.rings {
                queue.drain_ring(ring, ring_bytes, block);
            }
            let drained = queue.hand_on(&mut blocks.filled);
            blocks.emptied.append(&mut queue.free);
            return drained;
        };

        let mut queue = queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        queue.take_back(&mut blocks.emptied);
        for ring in &self.rings {
            queue.drain_ring(ring, ring_bytes, block);
        }
        queue.hand_on(&mut blocks.filled)
    }

    fn block_bytes(&self) -> usize {
        (4 * self.pages * sys::page_size()).max(BLOCK_BYTES)
    }

    pub fn read_while<R>(&self, work: impl FnOnce() -> R) -> R {
        let Some(queue) = &self.buffered else {
            return work();
        };
        let (block, ring_bytes) = (self.block_bytes(), self.pages * sys::page_size());
        let stop = AtomicBool::new(false);
        let poller = sys::Poller::new(&self.rings).ok();
        let reading = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let reader = scope.spawn(|| {
                reading.wait();
                let began = sys::thread_cpu();
                let sampling: Vec<u64> = if self.pause_on_full {
                    self.rings
                        .iter()
                        .map(sys::Ring::fd)
                        .chain(&self.followers)
                        .filter_map(|fd| sys::id(fd).ok())
                        .collect()
                } else {
                    Vec::new()
                };
                let mut drained = Vec::new();
                while !stop.load(Ordering::Relaxed) {
                    let mut queue = queue
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let pass = Pass::of(
                        queue.bytes,
                        self.queue_bytes,
                        self.pause_on_full,
                        !sampling.is_empty(),
                    );
                    let Pass::Drain {
                        behind: now_behind,
                        shorten,
                    } = pass
                    else {
                        drop(queue);
                        std::thread::sleep(self.reader_wait);
                        continue;
                    };
                    self.fall_behind(now_behind);
                    let before = queue.bytes;
                    if shorten {
                        for ring in &self.rings {
                            drained.clear();
                            ring.drain(&mut drained);
                            let into = queue.room(drained.len(), block);
                            let before = into.len();
                            let shortened = decode::shorten_stacks(
                                &drained,
                                &sampling,
                                SHORT_STACK_BYTES,
                                into,
                            );
                            queue.bytes += into.len() - before;
                            self.shortened.fetch_add(shortened, Ordering::Relaxed);
                        }
                    } else {
                        for ring in &self.rings {
                            queue.drain_ring(ring, ring_bytes, block);
                        }
                    }
                    let grew = queue.bytes > before;
                    drop(queue);
                    if grew {
                        sys::signal(&self.handed);
                    }
                    let elapsed = sys::thread_cpu().saturating_sub(began);
                    self.reader_cpu.store(
                        u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX),
                        Ordering::Relaxed,
                    );
                    match &poller {
                        Some(poller) => poller.wait(&self.rings, READER_TICK),
                        None => {
                            sys::poll(self.rings.iter().map(|ring| ring.fd().as_fd()), READER_TICK);
                        }
                    }
                }
            });
            reading.wait();
            let guard = StopReader(&stop, poller.as_ref());
            let result = work();
            drop(guard);
            let _ = reader.join();
            result
        })
    }

    fn fall_behind(&self, behind: bool) {
        let mut gate = self
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if gate.holds(Hold::Behind) == behind {
            return;
        }
        if let Some(on) = gate.hold(Hold::Behind, behind, sys::monotonic()) {
            self.enable_sampling(on);
        }
        drop(gate);
        if behind {
            record!("perf: the reader is behind, sampling is off");
        } else {
            record!("perf: the reader caught up");
        }
    }

    pub fn behind(&self, until: Option<u64>) -> Vec<(u64, u64)> {
        self.gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take_spans(until)
    }

    #[cfg(test)]
    pub(crate) const fn set_queue_bytes(&mut self, bytes: usize) {
        debug_assert!(bytes <= QUEUE_BYTES);
        self.queue_bytes = bytes;
    }

    #[cfg(test)]
    pub(crate) fn queued(&self) -> usize {
        self.buffered.as_ref().map_or(0, |queue| {
            queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .bytes
        })
    }

    #[cfg(test)]
    fn is_behind(&self) -> bool {
        self.gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .holds(Hold::Behind)
    }

    #[cfg(test)]
    pub(crate) fn rings_read(&self) -> bool {
        self.rings.iter().all(|ring| ring.unread() == 0)
    }

    #[cfg(test)]
    pub(crate) fn holding_the_reader<R>(&self, work: impl FnOnce() -> R) -> R {
        let held = self.buffered.as_ref().map(|queue| {
            queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        let result = work();
        drop(held);
        result
    }

    pub fn reader_cpu(&self) -> Duration {
        Duration::from_nanos(self.reader_cpu.load(Ordering::Relaxed))
    }

    pub fn untrusted(&self) -> u64 {
        self.rings.iter().map(sys::Ring::untrusted).sum()
    }

    pub fn wait(&self, timeout: Duration, exit: BorrowedFd<'_>) {
        if self.buffered.is_some() {
            sys::poll([self.handed.as_fd(), exit], timeout);
            sys::clear(&self.handed);
        } else {
            sys::poll(
                self.rings
                    .iter()
                    .map(|ring| ring.fd().as_fd())
                    .chain([exit]),
                timeout,
            );
        }
    }

    pub fn counts(&self) -> Counts {
        let sum = |fds: &mut dyn Iterator<Item = &OwnedFd>| {
            fds.filter_map(|fd| sys::counts(fd).ok())
                .fold((0, 0), |(count, lost), (more, also)| {
                    (count + more, lost + also)
                })
        };
        let (counted, lost) = sum(&mut self.rings.iter().map(sys::Ring::fd).chain(&self.followers));
        let (_, sideband) = sum(&mut self.sideband.iter());
        let (lost_samples, lost_records) = if self.sideband.is_empty() {
            (0, lost)
        } else {
            (lost, sideband)
        };

        Counts {
            counted,
            lost_samples,
            lost_records,
            untrusted: self.untrusted(),
            shortened: self.shortened.load(Ordering::Relaxed),
        }
    }

    pub fn start(&self) {
        let gate = self
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        debug_assert!(
            !gate.holds(Hold::Ended),
            "events started after they stopped"
        );
        self.enable_sampling(gate.held == 0);
        drop(gate);
        for fd in &self.sideband {
            let _ = sys::enable(fd, true);
        }
    }

    pub fn stop(&self) {
        let mut gate = self
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(on) = gate.hold(Hold::Ended, true, sys::monotonic()) {
            self.enable_sampling(on);
        }
        drop(gate);
        for fd in &self.sideband {
            let _ = sys::enable(fd, false);
        }
    }

    pub fn hold(&self, who: Hold, held: bool) {
        debug_assert!(
            who == Hold::Snapshot,
            "{who:?} is held by the events themselves"
        );
        let mut gate = self
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(on) = gate.hold(who, held, sys::monotonic()) {
            self.enable_sampling(on);
        }
    }

    fn enable_sampling(&self, on: bool) {
        for fd in self.rings.iter().map(sys::Ring::fd).chain(&self.followers) {
            let _ = sys::enable(fd, on);
        }
    }
}

struct StopReader<'a>(&'a AtomicBool, Option<&'a sys::Poller>);

impl Drop for StopReader<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
        if let Some(poller) = self.1 {
            poller.wake();
        }
    }
}

fn collection_buffer(options: &EventOptions) -> Option<Mutex<Queue>> {
    (!matches!(options.source, Source::Mappings)).then(Mutex::default)
}

const READER_TICK: Duration = Duration::from_millis(100);

fn reader_wait(options: &EventOptions) -> Duration {
    match options.source {
        Source::Clock { period } if period > Duration::from_millis(2) => Duration::from_millis(100),
        _ => Duration::from_micros(100),
    }
}

const fn sideband_flags(switches: bool) -> u64 {
    use sys::flag;
    flag::MMAP
        | flag::MMAP2
        | flag::COMM
        | flag::COMM_EXEC
        | flag::TASK
        | if switches { flag::CONTEXT_SWITCH } else { 0 }
}

fn attr(options: &EventOptions, start: u64) -> sys::Attr {
    use sys::flag;
    let mut attr = sys::Attr {
        size: sys::ATTR_SIZE,
        sample_type: decode::SAMPLE_TYPE,
        flags: start
            | flag::INHERIT
            | if options.kernel {
                0
            } else {
                flag::EXCLUDE_KERNEL
            }
            | flag::EXCLUDE_HV
            | flag::SAMPLE_ID_ALL
            | flag::EXCLUDE_CALLCHAIN_KERNEL
            | flag::USE_CLOCKID
            | flag::WATERMARK,
        read_format: sys::FORMAT_LOST,
        sample_regs_user: decode::REGISTERS,
        sample_stack_user: options.stack_bytes,
        clockid: libc::CLOCK_MONOTONIC,
        sample_max_stack: crate::unwind::sample::CHAIN_LIMIT,
        ..sys::Attr::default()
    };

    if options.source == Source::Mappings {
        attr.flags |= sideband_flags(options.switches);
    }

    match options.source {
        Source::Clock { period } => {
            attr.kind = sys::TYPE_SOFTWARE;
            attr.config = sys::COUNT_SW_CPU_CLOCK;
            attr.sample_period = u64::try_from(period.as_nanos()).unwrap_or(u64::MAX).max(1);
        }
        Source::Breakpoint { address, period } => {
            attr.kind = sys::TYPE_BREAKPOINT;
            if period > 1 {
                attr.sample_type &= !sys::sample::PERIOD;
            }
            attr.bp_type = sys::BREAKPOINT_EXECUTE;
            attr.bp_addr = address;
            attr.bp_len = 8;
            attr.sample_period = period.max(1);
        }
        Source::Mappings => {
            attr.kind = sys::TYPE_SOFTWARE;
            attr.config = sys::COUNT_SW_DUMMY;
        }
    }

    attr
}

fn sideband_attr(options: &EventOptions, start: u64) -> Option<sys::Attr> {
    use sys::flag;
    (options.source != Source::Mappings).then(|| sys::Attr {
        size: sys::ATTR_SIZE,
        kind: sys::TYPE_SOFTWARE,
        config: sys::COUNT_SW_DUMMY,
        sample_type: decode::SAMPLE_ID_TYPE,
        flags: start
            | flag::INHERIT
            | flag::EXCLUDE_HV
            | flag::SAMPLE_ID_ALL
            | flag::USE_CLOCKID
            | sideband_flags(options.switches),
        read_format: sys::FORMAT_LOST,
        clockid: libc::CLOCK_MONOTONIC,
        ..sys::Attr::default()
    })
}

fn into_rings(
    attr: &sys::Attr,
    pid: i32,
    cpus: &[u32],
    rings: &[sys::Ring],
) -> Result<Vec<OwnedFd>, OpenError> {
    let events = open_each(attr, pid, cpus)?;
    for ((fd, ring), &cpu) in events.iter().zip(rings).zip(cpus) {
        sys::set_output(fd, ring.fd()).map_err(|error| OpenError::Event { cpu, error })?;
    }

    Ok(events)
}

struct Leased {
    pages: usize,
    lease: lease::Lease,
}

impl Leased {
    fn take(options: &EventOptions, cpus: usize) -> Self {
        let fewest = min_pages(options.stack_bytes);
        let mut pages = options.ring_pages.next_power_of_two().max(fewest);

        loop {
            let bytes = (pages + 1) * sys::page_size() * cpus;
            let wanted = match options.source {
                _ if pages != fewest => lease::Wanted::Try,
                Source::Mappings => lease::Wanted::Beside,
                _ => lease::Wanted::Wait,
            };
            match lease::take(bytes, wanted) {
                Some(lease) => return Self { pages, lease },
                None => pages = (pages / 2).max(fewest),
            }
        }
    }
}

fn rings(
    mut attr: sys::Attr,
    pid: i32,
    cpus: &[u32],
    options: &EventOptions,
    leased: Leased,
) -> Result<(Vec<sys::Ring>, usize, u32, lease::Lease), OpenError> {
    let fewest = min_pages(options.stack_bytes);
    let Leased { mut pages, lease } = leased;

    loop {
        attr.wakeup_watermark = u32::try_from(pages * sys::page_size() / 4).unwrap_or(u32::MAX);
        let rings: std::io::Result<Vec<sys::Ring>> = open_each(&attr, pid, cpus)?
            .into_iter()
            .map(|fd| sys::Ring::map(fd, pages))
            .collect();

        match rings {
            Ok(rings) => return Ok((rings, pages, attr.wakeup_watermark, lease)),
            Err(error) if error.raw_os_error() == Some(libc::EPERM) => {
                if pages == fewest {
                    return Err(OpenError::RingBudget { pages });
                }

                pages /= 2;
            }
            Err(error) => return Err(OpenError::Ring(error)),
        }
    }
}

fn threads(pid: u32) -> std::io::Result<Vec<u32>> {
    let mut threads = Vec::new();
    for entry in std::fs::read_dir(format!("/proc/{pid}/task"))? {
        if let Some(tid) = entry?
            .file_name()
            .to_str()
            .and_then(|name| name.parse().ok())
        {
            threads.push(tid);
        }
    }

    threads.sort_unstable();
    Ok(threads)
}

pub fn descendants(pid: u32) -> Vec<u32> {
    let mut children: std::collections::BTreeMap<u32, Vec<u32>> = std::collections::BTreeMap::new();
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(process) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{process}/stat")) else {
            continue;
        };
        let parent = stat
            .rfind(')')
            .and_then(|end| stat[end + 1..].split_whitespace().nth(1)?.parse().ok());
        if let Some(parent) = parent {
            children.entry(parent).or_default().push(process);
        }
    }

    let mut tree = vec![pid];
    let mut next = 0;
    while let Some(&process) = tree.get(next) {
        tree.extend(children.remove(&process).unwrap_or_default());
        next += 1;
    }
    tree.remove(0);
    tree
}

fn raise_descriptor_limit() {
    use nix::sys::resource::{Resource, getrlimit, setrlimit};
    if let Ok((_, hard)) = getrlimit(Resource::RLIMIT_NOFILE) {
        let _ = setrlimit(Resource::RLIMIT_NOFILE, hard, hard);
    }
}

fn open_each(attr: &sys::Attr, pid: i32, cpus: &[u32]) -> Result<Vec<OwnedFd>, OpenError> {
    cpus.iter()
        .map(|&cpu| {
            sys::open(attr, pid, i32::try_from(cpu).unwrap_or(i32::MAX)).map_err(
                |error| match error.raw_os_error() {
                    Some(libc::EACCES | libc::EPERM) => OpenError::Denied {
                        paranoid: std::fs::read_to_string("/proc/sys/kernel/perf_event_paranoid")
                            .map_or_else(|_| "unknown".into(), |text| text.trim().to_owned()),
                        error,
                    },
                    _ => OpenError::Event { cpu, error },
                },
            )
        })
        .collect()
}

pub fn online_cpus() -> std::io::Result<Vec<u32>> {
    let text = std::fs::read_to_string("/sys/devices/system/cpu/online")?;
    let mut cpus = Vec::new();

    for part in text.trim().split(',').filter(|part| !part.is_empty()) {
        let (first, last) = part.split_once('-').unwrap_or((part, part));
        let parse = |text: &str| {
            text.parse::<u32>()
                .map_err(|_| std::io::Error::other(format!("CPU list '{}'", text.trim())))
        };

        cpus.extend(parse(first)?..=parse(last)?);
    }

    Ok(cpus)
}

#[derive(Debug)]
pub struct Attached {
    pub events: Events,
    pub process: ProcessWatch,
}

#[derive(Debug)]
pub struct ProcessWatch(OwnedFd);

impl AsFd for ProcessWatch {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

pub fn attach(
    pid: u32,
    options: &EventOptions,
    budget: usize,
    held: bool,
) -> Result<Attached, OpenError> {
    let process = super::native::pidfd_open(nix::unistd::Pid::from_raw(
        i32::try_from(pid).unwrap_or(i32::MAX),
    ))
    .map_err(|error| OpenError::NoProcess(error.into()))?;
    let events = if held {
        Events::attach_held(pid, options, budget)?
    } else {
        Events::attach(pid, options, budget)?
    };

    Ok(Attached {
        events,
        process: ProcessWatch(process),
    })
}

pub fn program_bias(pid: u32) -> Option<crate::LoadBias> {
    use std::os::unix::fs::MetadataExt;
    let inode = std::fs::metadata(format!("/proc/{pid}/exe")).ok()?.ino();
    let mapped = mappings(pid)
        .ok()?
        .into_iter()
        .find(|mapped| mapped.inode == inode)?;

    identify(&decode::Mmap {
        pid: crate::model::Pid::new(pid),
        start: mapped.start,
        length: mapped.end - mapped.start,
        file_offset: mapped.file_offset,
        inode: Some(mapped.inode),
        executable: true,
        path: &mapped.path,
        time: 0,
    })
    .map(|(_, bias)| bias)
}

#[derive(Debug)]
pub struct Launched {
    pub pid: u32,
    pub events: Events,

    pub exit: JoinHandle<std::io::Result<ExitStatus>>,

    pub killer: Killer,

    pub executed: std::sync::mpsc::Receiver<()>,

    pub gate: LaunchGate,
}

#[derive(Debug)]
pub struct LaunchGate {
    #[cfg_attr(not(debug_assertions), expect(dead_code))]
    pid: u32,
    go: OwnedFd,
}

impl LaunchGate {
    pub fn open(self) -> std::io::Result<()> {
        let opened = write_all(&self.go, &[1]);
        match &opened {
            Ok(()) => record!("perf launch: {} opened its gate", self.pid),
            #[cfg_attr(not(debug_assertions), expect(unused_variables))]
            Err(error) => record!("perf launch: {}'s gate did not open: {error}", self.pid),
        }

        opened
    }
}

#[derive(Debug)]
pub struct Killer(OwnedFd);

impl Killer {
    pub fn kill(&self) {
        let _ = super::native::pidfd_send_signal(&self.0, super::signals::Signal::SIGKILL);
    }
}

impl AsFd for Killer {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaunchOptions {
    pub no_address_randomization: bool,

    pub filter: Option<Vec<super::syscalls::seccomp::Instruction>>,
}

pub fn launch(
    mut command: Command,
    options: &EventOptions,
    launch: &LaunchOptions,
    held: impl FnOnce(u32, &Events) -> Result<(), String>,
) -> Result<Launched, LaunchError> {
    let cpus = online_cpus().map_err(|error| LaunchError::Events(OpenError::Cpus(error)))?;
    let leased = Leased::take(options, cpus.len());
    let (pid_read, pid_write) = pipe()?;
    let (go_read, go_write) = pipe()?;
    gate(&mut command, [&pid_write, &go_read, &go_write], launch);
    let (executing, executed) = std::sync::mpsc::channel();
    let spawner = std::thread::Builder::new()
        .name("microscope-record-child".into())
        .spawn(move || {
            let spawned = command.spawn();

            drop((pid_write, go_read));
            let child = spawned?;
            let _ = executing.send(());

            let exit = super::native::pidfd_open(nix::unistd::Pid::from_raw(
                i32::try_from(child.id()).unwrap_or(i32::MAX),
            ));
            if let Ok(exit) = &exit {
                sys::wait_readable(exit);
            }

            super::reaped::REAPED.wait_unheld(child.id());
            reap(child)
        })
        .map_err(LaunchError::Spawn)?;

    let mut pid = [0_u8; 4];
    if read_exact(&pid_read, &mut pid).is_err() {
        let error = match spawner.join() {
            Ok(Err(error)) => error,
            _ => std::io::Error::other("the program did not start"),
        };

        return Err(LaunchError::Spawn(error));
    }

    let pid = u32::from_le_bytes(pid);
    record!("perf launch: {pid} waits at its gate");

    super::reaped::REAPED.watch(pid);

    let killer = match super::native::pidfd_open(nix::unistd::Pid::from_raw(
        i32::try_from(pid).unwrap_or(i32::MAX),
    )) {
        Ok(descriptor) => Killer(descriptor),
        Err(error) => {
            drop(go_write);
            let _ = spawner.join();
            return Err(LaunchError::Spawn(error.into()));
        }
    };

    let events = match Events::open_leased(pid, options, leased) {
        Ok(events) => events,
        Err(error) => {
            record!("perf launch: closing {pid}'s gate unopened: {error}");

            drop(go_write);
            let _ = spawner.join();
            return Err(LaunchError::Events(error));
        }
    };

    if let Err(why) = held(pid, &events) {
        record!("perf launch: closing {pid}'s gate unopened: {why}");
        drop(go_write);
        let _ = spawner.join();
        return Err(LaunchError::Held(why));
    }

    Ok(Launched {
        pid,
        events,
        exit: spawner,
        killer,
        executed,
        gate: LaunchGate { pid, go: go_write },
    })
}

const REAPED_PATIENCE: Duration = Duration::from_secs(5);

fn reap(mut child: std::process::Child) -> std::io::Result<std::process::ExitStatus> {
    use std::os::unix::process::ExitStatusExt as _;
    let pid = child.id();
    match child.wait() {
        Err(error) if error.raw_os_error() == Some(libc::ECHILD) => super::reaped::REAPED
            .take(pid, REAPED_PATIENCE)
            .map(std::process::ExitStatus::from_raw)
            .ok_or_else(|| {
                std::io::Error::other(format!(
                    "program {pid} was reaped by a wait that left no status: {error}"
                ))
            }),
        waited => {
            super::reaped::REAPED.forget(pid);
            waited
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    #[error("starting the program failed: {0}")]
    Spawn(#[from] std::io::Error),

    #[error(transparent)]
    Events(OpenError),

    #[error("{0}")]
    Held(String),
}

fn pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC).map_err(std::io::Error::from)
}

fn read_exact(fd: &OwnedFd, buffer: &mut [u8]) -> std::io::Result<()> {
    let mut file = std::fs::File::from(fd.try_clone()?);
    std::io::Read::read_exact(&mut file, buffer)
}

fn write_all(fd: &OwnedFd, buffer: &[u8]) -> std::io::Result<()> {
    let mut file = std::fs::File::from(fd.try_clone()?);
    std::io::Write::write_all(&mut file, buffer)
}

fn gate(
    command: &mut Command,
    [pid_write, go_read, go_write]: [&OwnedFd; 3],
    launch: &LaunchOptions,
) {
    let filter = launch
        .filter
        .as_ref()
        .map(|program| program.iter().copied().map(Into::into).collect());
    super::sys::before_exec(
        command,
        super::sys::BeforeExec {
            dies_with_parent: true,
            fixed_addresses: launch.no_address_randomization,
            gate: Some(super::sys::Gate {
                pid: pid_write.as_raw_fd(),
                go_read: go_read.as_raw_fd(),
                go_write: go_write.as_raw_fd(),
            }),
            filter,
            ..Default::default()
        },
    );
}

pub fn own_vdso() -> Option<(Vec<u8>, u64)> {
    let maps = std::fs::read_to_string("/proc/self/maps").ok()?;
    let range = super::vdso::vdso_mapping(&maps)?;
    let memory = std::fs::File::open("/proc/self/mem").ok()?;
    let image = super::vdso::read_memory_image(range.start, range.end, |address, buffer| {
        std::os::unix::fs::FileExt::read_exact_at(&memory, buffer, address).is_ok()
    })
    .ok()?;

    let link = image
        .load_bias
        .wrapping_unload(crate::VirtualAddress::new(range.start));
    Some((image.data, link.get()))
}

pub fn is_vdso(mmap: &decode::Mmap<'_>) -> bool {
    mmap.executable && mmap.path == super::vdso::VDSO_NAME.as_bytes()
}

pub fn identify(mmap: &decode::Mmap<'_>) -> Option<(PathBuf, crate::LoadBias)> {
    use std::os::unix::ffi::OsStrExt;
    if !mmap.executable || !mmap.path.starts_with(b"/") || jit::is_anonymous_code(mmap) {
        return None;
    }

    let deleted = mmap.path.strip_suffix(b" (deleted)");
    let path = std::path::Path::new(std::ffi::OsStr::from_bytes(deleted.unwrap_or(mmap.path)));
    let pid = mmap.pid;
    let mut candidates = Vec::with_capacity(4);
    if deleted.is_none() {
        candidates.push(path.to_owned());
        let relative = path.strip_prefix("/").unwrap_or(path);
        candidates.push(PathBuf::from(format!("/proc/{pid}/root")).join(relative));
    }

    let end = mmap.start.saturating_add(mmap.length);
    candidates.push(PathBuf::from(format!(
        "/proc/{pid}/map_files/{:x}-{end:x}",
        mmap.start
    )));
    candidates.push(PathBuf::from(format!("/proc/{pid}/exe")));
    let inode = mmap.inode?;

    candidates.into_iter().find_map(|candidate| {
        identify_mapped_module(&ModuleMapping {
            path: candidate,
            inode,
            start: mmap.start,
            file_offset: mmap.file_offset,
            deleted: false,
            executable: true,
        })
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mapped {
    pub start: u64,
    pub end: u64,
    pub file_offset: u64,
    pub inode: u64,
    pub executable: bool,

    pub writable: bool,
    pub path: Vec<u8>,
}

pub fn mappings(pid: u32) -> std::io::Result<Vec<Mapped>> {
    let maps = std::fs::read(format!("/proc/{pid}/maps"))?;
    let mut mappings = Vec::new();

    for line in maps.split(|byte| *byte == b'\n') {
        let mut fields = line.splitn(6, |byte| *byte == b' ');
        let (Some(range), Some(permissions), Some(offset), Some(_device), Some(inode)) = (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        ) else {
            continue;
        };

        let path = fields.next().unwrap_or_default().trim_ascii_start();
        let hex = |text: &[u8]| u64::from_str_radix(std::str::from_utf8(text).ok()?, 16).ok();
        let mut bounds = range.splitn(2, |byte| *byte == b'-');
        let (Some(start), Some(end), Some(file_offset)) = (
            bounds.next().and_then(hex),
            bounds.next().and_then(hex),
            hex(offset),
        ) else {
            continue;
        };

        let executable = permissions.get(2) == Some(&b'x');
        let inode = std::str::from_utf8(inode)
            .ok()
            .and_then(|inode| inode.parse().ok());

        let path = if path.is_empty() { b"//anon" } else { path };
        if executable {
            mappings.push(Mapped {
                start,
                end,
                file_offset,
                inode: inode.unwrap_or(0),
                executable,
                writable: permissions.get(1) == Some(&b'w'),
                path: path.to_vec(),
            });
        }
    }

    Ok(mappings)
}

pub fn mapping_at(pid: u32, address: u64) -> std::io::Result<Option<Mapped>> {
    let maps = std::fs::File::open(format!("/proc/{pid}/maps"))?;
    match sys::executable_vma(&maps, address) {
        Ok(vma) => Ok(vma.map(|vma| Mapped {
            start: vma.start,
            end: vma.end,
            file_offset: vma.offset,
            inode: vma.inode,
            executable: true,
            writable: vma.writable,
            path: if vma.name.is_empty() {
                b"//anon".to_vec()
            } else {
                vma.name
            },
        })),
        Err(error) if error.raw_os_error() == Some(nix::libc::ENOTTY) => Ok(mappings(pid)?
            .into_iter()
            .find(|mapped| (mapped.start..mapped.end).contains(&address))),
        Err(error) => Err(error),
    }
}

pub fn is_anonymous(mapping: &Mapped) -> bool {
    mapping.inode == 0 && (mapping.path == b"//anon" || mapping.path.starts_with(b"[anon:"))
}

pub fn copy_of<K: Copy>(
    pid: u32,
    range: &std::ops::Range<u64>,
    sources: &[crate::unwind::code_copies::Source<'_, K>],
) -> Option<crate::unwind::code_copies::Proven<K, ()>> {
    use crate::unwind::code_copies::{PROBE, compare_bytes, prove};

    const CHUNK: usize = 256 * 1024;

    let mut read = |address: u64, length: usize| {
        let mut bytes = vec![0; length];
        let read = read_memory(pid, address, &mut bytes).ok()?;
        bytes.truncate(read);
        Some(bytes)
    };
    let mut probe = |address| {
        let mut bytes = [0; PROBE];
        (read_memory(pid, address, &mut bytes).ok()? == PROBE).then_some(bytes)
    };
    let mut compare =
        |source, copy, limit| (compare_bytes(&mut read, source, copy, limit, CHUNK), ());
    prove(range, sources, &mut probe, &mut compare)
}

pub fn stored_code(
    pid: u32,
    info: &crate::debug_info::DebugInfo,
    loaded: crate::LoadedModule,
    mapped: std::ops::Range<u64>,
) -> Vec<(crate::ImageAddress, crate::ImageAddress)> {
    stored_code_of(&code_keepers(info), pid, loaded, mapped)
}

pub fn code_keepers(
    info: &crate::debug_info::DebugInfo,
) -> Vec<std::sync::Arc<dyn crate::runtime_model::RuntimeModel>> {
    use crate::runtime_model::RuntimeImage;

    let image: std::sync::Arc<dyn RuntimeImage + Send + Sync> =
        std::sync::Arc::clone(&info.image) as _;
    if image.v8().is_none() {
        return Vec::new();
    }
    crate::runtime_model::detect(&image)
        .into_iter()
        .flatten()
        .collect()
}

pub fn stored_code_of(
    keepers: &[std::sync::Arc<dyn crate::runtime_model::RuntimeModel>],
    pid: u32,
    loaded: crate::LoadedModule,
    mapped: std::ops::Range<u64>,
) -> Vec<(crate::ImageAddress, crate::ImageAddress)> {
    if keepers.is_empty() {
        return Vec::new();
    }
    let stop = crate::profiler::LiveStop::memory(pid, loaded.load_bias);
    keepers
        .iter()
        .flat_map(|model| model.process_code(&stop))
        .filter(|range| mapped.start <= range.start.get() && range.end.get() <= mapped.end)
        .map(|range| {
            (
                loaded.load_bias.wrapping_unload(range.start),
                loaded.load_bias.wrapping_unload(range.end),
            )
        })
        .collect()
}

pub fn read_memory(pid: u32, address: u64, buffer: &mut [u8]) -> std::io::Result<usize> {
    let remote = [nix::sys::uio::RemoteIoVec {
        base: usize::try_from(address).map_err(std::io::Error::other)?,
        len: buffer.len(),
    }];
    let mut local = [std::io::IoSliceMut::new(buffer)];
    nix::sys::uio::process_vm_readv(
        nix::unistd::Pid::from_raw(pid.cast_signed()),
        &mut local,
        &remote,
    )
    .map_err(std::io::Error::from)
}

pub fn read_memory_each(pid: u32, reads: &mut [(crate::VirtualAddress, &mut [u8])]) -> Vec<bool> {
    const MOST: usize = 1024;
    let mut filled = vec![false; reads.len()];
    let mut next = 0;
    while next < reads.len() {
        let end = reads.len().min(next + MOST);
        #[expect(clippy::indexing_slicing, reason = "next < end <= reads.len()")]
        let batch = &mut reads[next..end];
        let remote: Option<Vec<nix::sys::uio::RemoteIoVec>> = batch
            .iter()
            .map(|(address, bytes)| {
                Some(nix::sys::uio::RemoteIoVec {
                    base: usize::try_from(address.get()).ok()?,
                    len: bytes.len(),
                })
            })
            .collect();
        let Some(remote) = remote else {
            break;
        };
        let mut local: Vec<std::io::IoSliceMut<'_>> = batch
            .iter_mut()
            .map(|(_, bytes)| std::io::IoSliceMut::new(bytes))
            .collect();
        let mut left = match nix::sys::uio::process_vm_readv(
            nix::unistd::Pid::from_raw(pid.cast_signed()),
            &mut local,
            &remote,
        ) {
            Ok(read) => read,
            Err(nix::errno::Errno::EFAULT) => 0,
            Err(_) => break,
        };
        drop(local);
        let mut whole = 0;
        for (_, bytes) in batch.iter() {
            if left < bytes.len() {
                break;
            }
            left -= bytes.len();
            whole += 1;
        }
        for slot in filled.iter_mut().skip(next).take(whole) {
            *slot = true;
        }
        next += whole;
        if next < end {
            next += 1;
        }
    }
    filled
}

pub fn write_memory(pid: u32, address: u64, bytes: &[u8]) -> std::io::Result<()> {
    let remote = [nix::sys::uio::RemoteIoVec {
        base: usize::try_from(address).map_err(std::io::Error::other)?,
        len: bytes.len(),
    }];
    let written = nix::sys::uio::process_vm_writev(
        nix::unistd::Pid::from_raw(pid.cast_signed()),
        &[std::io::IoSlice::new(bytes)],
        &remote,
    )?;
    if written == bytes.len() {
        Ok(())
    } else {
        Err(std::io::Error::from(std::io::ErrorKind::WriteZero))
    }
}

pub fn tsc_is_the_clock() -> bool {
    std::fs::read_to_string("/sys/devices/system/clocksource/clocksource0/current_clocksource")
        .is_ok_and(|source| source.trim() == "tsc")
}

pub fn tsc_anchor() -> (u64, u64) {
    let monotonic = || {
        nix::time::clock_gettime(nix::time::ClockId::CLOCK_MONOTONIC).map_or(0, |time| {
            u64::try_from(time.tv_sec()).unwrap_or(0) * 1_000_000_000
                + u64::try_from(time.tv_nsec()).unwrap_or(0)
        })
    };

    let mut best = (u64::MAX, 0, 0);

    for _ in 0..4 {
        let before = monotonic();
        let tsc = sys::tsc();
        let after = monotonic();
        let width = after.saturating_sub(before);
        if width < best.0 {
            best = (width, tsc, before + width / 2);
        }
    }

    (best.1, best.2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_queue_pauses_a_clocks_sampling_but_never_a_breakpoints() {
        const LIMIT: usize = 64;
        let drain = |behind, shorten| Pass::Drain { behind, shorten };

        let breakpoint = |queued| Pass::of(queued, LIMIT, false, false);
        assert_eq!(breakpoint(0), drain(false, false));
        assert_eq!(breakpoint(LIMIT - 1), drain(false, false));
        assert_eq!(breakpoint(LIMIT), Pass::Wait);
        assert_eq!(breakpoint(2 * LIMIT), Pass::Wait);

        let clock = |queued| Pass::of(queued, LIMIT, true, true);
        assert_eq!(clock(47), drain(false, false));
        assert_eq!(clock(48), drain(false, true));
        assert_eq!(clock(LIMIT - 1), drain(false, true));
        assert_eq!(clock(LIMIT), drain(true, true));

        assert_eq!(Pass::of(LIMIT, LIMIT, true, false), drain(true, false));

        let queued = |source| {
            let options = EventOptions {
                source,
                stack_bytes: 512,
                kernel: false,
                ring_pages: 1,
                switches: false,
            };
            collection_buffer(&options).is_some()
        };
        let period = Duration::from_millis(1);
        assert!(queued(Source::Clock { period }));
        assert!(queued(Source::Breakpoint {
            address: 0x1000,
            period: 1
        }));
        assert!(!queued(Source::Mappings));
    }

    #[test]
    fn sampling_is_on_while_nothing_holds_it_off() {
        use Hold::{Behind, Ended, Snapshot};

        let mut gate = Gate::default();
        let mut on = true;
        let mut seen = Vec::new();
        for (time, who, held) in [
            (30, Snapshot, true),
            (60, Snapshot, false),
            (70, Behind, true),
            (80, Snapshot, true),
            (90, Behind, false),
            (100, Snapshot, false),
            (110, Snapshot, true),
            (120, Behind, true),
            (130, Snapshot, false),
            (140, Behind, false),
            (150, Behind, true),
            (160, Ended, true),
            (170, Behind, false),
        ] {
            if let Some(now) = gate.hold(who, held, time) {
                assert_ne!(now, on, "{who:?} at {time} toggled to the same state");
                on = now;
            }
            seen.push((time, on));
        }

        assert_eq!(
            seen,
            [
                (30, false),
                (60, true),
                (70, false),
                (80, false),
                (90, false),
                (100, true),
                (110, false),
                (120, false),
                (130, false),
                (140, true),
                (150, false),
                (160, false),
                (170, false),
            ]
        );
    }

    fn mapping_end() -> u64 {
        let maps = std::fs::read_to_string("/proc/self/maps").expect("read maps");
        let ranges: Vec<(u64, u64, bool)> = maps
            .lines()
            .filter_map(|line| {
                let (range, rest) = line.split_once(' ')?;
                let (start, end) = range.split_once('-')?;
                Some((
                    u64::from_str_radix(start, 16).ok()?,
                    u64::from_str_radix(end, 16).ok()?,
                    rest.starts_with('r'),
                ))
            })
            .collect();
        ranges
            .windows(2)
            .find(|pair| pair[0].2 && pair[1].0 > pair[0].1 + 0x1000)
            .map(|pair| pair[0].1)
            .expect("a readable mapping with a gap after it")
    }

    #[test]
    fn the_mapping_at_an_address_is_its_line_of_the_map() {
        let pid = std::process::id();
        let all = mappings(pid).expect("read maps");
        assert!(all.len() > 1);

        for mapped in all.iter().filter(|mapped| mapped.path != b"[vsyscall]") {
            for address in [mapped.start, mapped.end - 1] {
                assert_eq!(
                    mapping_at(pid, address).expect("query").as_ref(),
                    Some(mapped)
                );
            }
        }

        let stack = 0_u64;
        assert_eq!(
            mapping_at(pid, (&raw const stack).addr() as u64).expect("query"),
            None
        );
    }

    #[test]
    fn open_errors_name_their_cause_once() {
        let cause = || std::io::Error::from_raw_os_error(libc::EINVAL);
        for error in [
            OpenError::Denied {
                paranoid: "2".into(),
                error: cause(),
            },
            OpenError::Event {
                cpu: 0,
                error: cause(),
            },
            OpenError::Ring(cause()),
            OpenError::Cpus(cause()),
            OpenError::NoProcess(cause()),
        ] {
            let shown = format!("{:#}", anyhow::Error::from(error));
            assert_eq!(shown.matches("Invalid argument").count(), 1, "{shown}");
        }
    }

    #[test]
    fn a_full_ring_counts_the_samples_it_lost_apart_from_other_records() {
        const PERIOD_NS: u64 = 1_000_000;
        let options = EventOptions {
            source: Source::Clock {
                period: Duration::from_nanos(PERIOD_NS),
            },
            stack_bytes: 512,
            kernel: false,
            ring_pages: 1,
            switches: true,
        };
        let program = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("build/test-programs/profile-switches");
        let launched = launch(
            Command::new(program),
            &options,
            &LaunchOptions::default(),
            |_, _| Ok(()),
        )
        .expect("launch the program");
        launched.gate.open().expect("open the gate");
        let status = launched
            .exit
            .join()
            .expect("the launch thread ends")
            .expect("the program exits");
        assert!(status.success(), "{status}");

        let counts = launched.events.counts();
        let possible = counts.counted / PERIOD_NS + launched.events.cpus.len() as u64;
        assert!(
            (1..=possible).contains(&counts.lost_samples),
            "lost {counts:?}, but {possible} samples were possible"
        );
        assert!(
            counts.lost_records > 10 * possible,
            "lost {counts:?}, though the ring was never read"
        );
    }

    #[test]
    fn a_reader_that_falls_behind_shortens_stacks_instead_of_pausing_sampling() {
        let (events, exit, gate, spinning) = spin_with_deep_copies();
        let status = events
            .read_while(|| {
                gate.open().expect("open the gate");
                until("stacks are shortened", || events.counts().shortened > 0);
                drop(spinning);
                exit.join()
            })
            .expect("the launch thread ends")
            .expect("the program exits");
        assert!(status.success(), "{status}");

        let counts = events.counts();
        assert_eq!(events.behind(Some(sys::monotonic())), []);
        assert!(counts.shortened > 0, "{counts:?}");

        let mut blocks = Blocks::default();
        events.drain_blocks(&mut blocks);
        assert!(blocks.filled.len() > 1, "{} blocks", blocks.filled.len());
        let (mut long, mut short) = (0, 0);
        for block in &blocks.filled {
            let mut rest = block.as_slice();
            while let Some(Ok((record, length))) = decode::next(rest) {
                rest = &rest[length..];
                if let decode::Record::Sample(sample) = record {
                    if sample.stack_asked > SHORT_STACK_BYTES as u64 {
                        long += 1;
                    } else {
                        assert_eq!(sample.stack_asked, SHORT_STACK_BYTES as u64);
                        assert!(
                            sample.user_chain().next().is_some(),
                            "a sample lost its chain"
                        );
                        short += 1;
                    }
                }
            }
            assert!(rest.is_empty(), "a record spans blocks");
        }
        assert_eq!(short, counts.shortened);
        assert!(
            long > 0 && long * 65528 < QUEUE_BYTES as u64,
            "{long} long samples"
        );
    }

    #[test]
    fn a_reader_far_behind_pauses_sampling_instead_of_losing_samples() {
        let (mut events, exit, gate, spinning) = spin_with_deep_copies();
        events.queue_bytes = 4 * 1024 * 1024;
        let (status, exited) = events.read_while(|| {
            gate.open().expect("open the gate");
            until("sampling pauses", || events.is_behind());
            drop(spinning);
            (exit.join(), sys::monotonic())
        });
        let status = status
            .expect("the launch thread ends")
            .expect("the program exits");
        assert!(status.success(), "{status}");

        let end = sys::monotonic();
        assert_eq!(events.behind(None), []);
        let spans = events.behind(Some(end));
        assert!(
            matches!(spans[..], [(start, until)] if start < exited && until == end),
            "{spans:?}, though the program exited at {exited} and the test ended at {end}"
        );
        assert_eq!(events.behind(Some(end)), []);
    }

    #[test]
    fn a_waiting_processor_wakes_when_the_reader_queues_samples() {
        let (events, exit, gate, spinning) = spin_with_deep_copies();
        let never = sys::handoff().expect("an eventfd");
        let (waited, queued, status) = events.read_while(|| {
            gate.open().expect("open the gate");
            let began = std::time::Instant::now();
            events.wait(Duration::from_secs(20), never.as_fd());
            let (waited, queued) = (began.elapsed(), events.queued());
            drop(spinning);
            (waited, queued, exit.join())
        });
        let status = status
            .expect("the launch thread ends")
            .expect("the program exits");
        assert!(status.success(), "{status}");
        assert!(queued > 0, "woke after {waited:?} with nothing queued");
        assert!(waited < Duration::from_secs(5), "waited {waited:?}");
    }

    fn until(what: &str, done: impl Fn() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while !done() {
            assert!(std::time::Instant::now() < deadline, "{what} within 20 s");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn spin_with_deep_copies() -> (
        Events,
        JoinHandle<std::io::Result<ExitStatus>>,
        LaunchGate,
        std::io::PipeWriter,
    ) {
        let options = EventOptions {
            source: Source::Clock {
                period: Duration::from_millis(1),
            },
            stack_bytes: 65528,
            kernel: false,
            ring_pages: 128,
            switches: false,
        };
        let program = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("build/test-programs/profile-switches");
        let (stop, spinning) = std::io::pipe().expect("a pipe");
        let mut command = Command::new(program);
        command.args(["0", "-1", "8"]).stdin(stop);
        let Launched {
            events, exit, gate, ..
        } = launch(command, &options, &LaunchOptions::default(), |_, _| Ok(()))
            .expect("launch the program");
        (events, exit, gate, spinning)
    }

    #[test]
    fn the_reader_does_not_spin_on_the_rings_of_an_exited_program() {
        let options = EventOptions {
            source: Source::Clock {
                period: Duration::from_millis(20),
            },
            stack_bytes: 512,
            kernel: false,
            ring_pages: 16,
            switches: false,
        };
        let Launched {
            events, exit, gate, ..
        } = launch(
            Command::new("true"),
            &options,
            &LaunchOptions::default(),
            |_, _| Ok(()),
        )
        .expect("launch the program");

        events.read_while(|| {
            gate.open().expect("open the gate");
            let status = exit.join().expect("the launch thread ends").expect("wait");
            assert!(status.success(), "{status}");
            std::thread::sleep(Duration::from_millis(500));
        });
        let cpu = events.reader_cpu();
        assert!(cpu < Duration::from_millis(20), "the reader took {cpu:?}");
    }

    #[test]
    fn a_program_paused_from_sampling_still_records_the_processes_it_starts() {
        let options = EventOptions {
            source: Source::Clock {
                period: Duration::from_millis(1),
            },
            stack_bytes: 512,
            kernel: false,
            ring_pages: 16,
            switches: false,
        };
        let (reader, mut writer) = std::io::pipe().expect("pipe");
        let (mut said, saying) = std::io::pipe().expect("pipe");
        let mut command = Command::new("sh");
        command
            .args(["-c", "echo ready; read line; (:); :"])
            .stdin(reader)
            .stdout(saying);
        let Launched {
            events, exit, gate, ..
        } = launch(command, &options, &LaunchOptions::default(), |_, _| Ok(()))
            .expect("launch the program");

        let status = events.read_while(|| {
            gate.open().expect("open the gate");
            let mut ready = [0_u8; 6];
            std::io::Read::read_exact(&mut said, &mut ready).expect("the shell starts");
            events.hold(Hold::Snapshot, true);
            let paused = sys::monotonic();
            std::io::Write::write_all(&mut writer, b"\n").expect("release the shell");
            (exit.join(), paused)
        });
        let (status, paused) = status;
        let status = status
            .expect("the launch thread ends")
            .expect("the program exits");
        assert!(status.success(), "{status}");

        let mut records = Vec::new();
        events.drain(&mut records);
        let mut forks = 0;
        let mut rest = records.as_slice();
        while let Some(Ok((record, length))) = decode::next(rest) {
            rest = &rest[length..];
            if record.time().is_none_or(|time| time < paused) {
                continue;
            }
            assert!(!matches!(record, decode::Record::Sample(_)), "{record:?}");
            forks += usize::from(matches!(record, decode::Record::Fork { .. }));
        }
        assert_eq!(forks, 1);
    }

    #[test]
    fn each_read_is_filled_whole_or_not_at_all_and_the_rest_read_on() {
        let pid = std::process::id();
        let end = mapping_end();
        let source: Vec<[u8; 8]> = (0..1500u64).map(u64::to_le_bytes).collect();
        let at = |index: usize| crate::VirtualAddress::new(source[index].as_ptr() as u64);

        let mut plan: Vec<(crate::VirtualAddress, usize, bool)> = (0..source.len())
            .map(|index| (at(index), 8, true))
            .collect();
        plan[1] = (crate::VirtualAddress::new(end - 8), 16, false);
        plan[3] = (crate::VirtualAddress::new(end + 8), 8, false);
        plan[4] = (crate::VirtualAddress::new(end - 8), 16, false);
        plan[1023] = (crate::VirtualAddress::new(end + 8), 8, false);
        plan[1024] = (crate::VirtualAddress::new(end - 8), 16, false);

        let mut buffers: Vec<Vec<u8>> = plan.iter().map(|&(_, len, _)| vec![0xee; len]).collect();
        let mut reads: Vec<(crate::VirtualAddress, &mut [u8])> = plan
            .iter()
            .zip(buffers.iter_mut())
            .map(|(&(address, _, _), buffer)| (address, buffer.as_mut_slice()))
            .collect();
        let filled = read_memory_each(pid, &mut reads);
        drop(reads);

        let expected: Vec<bool> = plan.iter().map(|&(_, _, whole)| whole).collect();
        assert_eq!(filled, expected);
        for (index, buffer) in buffers.iter().enumerate() {
            if expected[index] {
                assert_eq!(buffer.as_slice(), &source[index], "read {index}");
            }
        }
    }

    #[test]
    fn a_process_that_is_gone_fills_no_read() {
        let mut child = std::process::Command::new("true").spawn().expect("spawn");
        let pid = child.id();
        child.wait().expect("wait");
        let mut bytes = [0; 8];
        let mut reads = [(crate::VirtualAddress::new(0x40_0000), &mut bytes[..])];
        assert_eq!(read_memory_each(pid, &mut reads), [false]);
    }
}
