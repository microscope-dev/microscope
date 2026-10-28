//! Sampling processes with perf events, from outside them: no ptrace and
//! no code in the program. One event per CPU follows a process and every
//! thread and child it starts, and writes its samples to that CPU's ring.
//! A second, which samples nothing, writes what the process maps and names,
//! its threads' lifetimes, and their context switches to the same ring: the
//! kernel counts what each event loses, so the samples lost are counted
//! apart from the rest.

pub mod decode;
pub mod jit;
pub mod sys;

use std::os::fd::{AsRawFd, OwnedFd};
use std::path::PathBuf;
use std::process::{Command, ExitStatus};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use nix::libc;

use super::modules::{ModuleMapping, identify_mapped_module};

/// What makes a sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Every `period` of CPU time the program's threads spend in user code.
    Clock { period: Duration },

    /// Counts every execution at `address`, sampling every `period` hits.
    Breakpoint { address: u64, period: u64 },

    /// No samples: only what the program maps, and its threads.
    Mappings,
}

/// How events sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventOptions {
    pub source: Source,

    /// Bytes of stack each sample copies, a multiple of eight.
    pub stack_bytes: u32,

    /// Sample kernel CPU time as well as user CPU time.
    pub kernel: bool,

    /// Data pages of each ring, a power of two.
    pub ring_pages: usize,

    /// Whether the rings also say each time a thread leaves its CPU and
    /// comes back.
    pub switches: bool,
}

/// Why events could not be opened.
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

/// One event per online CPU, each with its ring.
#[derive(Debug)]
pub struct Events {
    rings: Vec<sys::Ring>,
    pages: usize,

    /// High-rate collection stays separate from symbol loading and unwinding.
    buffered: Option<Mutex<Vec<u8>>>,
    reader_cpu: AtomicU64,

    /// The CPU of each ring, in their order.
    cpus: Vec<u32>,

    /// Sampling events that write to the rings of others on their CPUs.
    followers: Vec<OwnedFd>,

    /// The events that write every record but samples, each to the ring
    /// of its CPU; none where the events sample nothing.
    sideband: Vec<OwnedFd>,
}

/// What a recording's events counted and lost.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// What the sampling events counted, in their source's units.
    pub counted: u64,

    /// Samples the rings had no room for.
    pub lost_samples: u64,

    /// Other records the rings had no room for: mappings, names, threads,
    /// and context switches.
    pub lost_records: u64,

    /// Times a ring's head could not be trusted.
    pub untrusted: u64,
}

/// The fields of each record a BPF program writes through an [`Output`],
/// in the kernel's order: those of a sample's that come first, then the
/// program's bytes.
pub const OUTPUT_SAMPLE_TYPE: u64 = sys::sample::IDENTIFIER
    | sys::sample::TID
    | sys::sample::TIME
    | sys::sample::CPU
    | sys::sample::RAW;

/// A CPU's BPF output event, which writes to that CPU's ring, and its
/// identifier, which each of its records begins with.
#[derive(Debug)]
pub struct Output {
    pub cpu: u32,
    pub fd: OwnedFd,
    pub id: u64,
}

/// The most bytes a sample of `stack_bytes` can take: its fixed fields,
/// the deepest chain with its context markers, every register, and the
/// stack with its sizes.
const fn max_sample_bytes(stack_bytes: u32) -> usize {
    const FIXED: usize = 8 * 8;
    const CHAIN: usize = 8 * (1 + 127 + 8);
    const REGISTERS: usize = 8 * (1 + decode::REGISTERS.count_ones() as usize);
    FIXED + CHAIN + REGISTERS + 16 + stack_bytes as usize
}

/// The fewest data pages, a power of two, that hold two of the largest
/// samples: a ring that holds none would lose every sample, and one that
/// holds a single sample loses the next before it can be drained.
fn min_pages(stack_bytes: u32) -> usize {
    (2 * max_sample_bytes(stack_bytes))
        .div_ceil(sys::page_size())
        .next_power_of_two()
}

impl Events {
    /// Opens disabled events for `pid`, its threads, and its children,
    /// which enable themselves when it next executes a program.
    pub fn open(pid: u32, options: &EventOptions) -> Result<Self, OpenError> {
        let start = sys::flag::DISABLED | sys::flag::ENABLE_ON_EXEC;
        let pid = i32::try_from(pid).map_err(|_| OpenError::Cpus(std::io::Error::other("pid")))?;
        let cpus = online_cpus().map_err(OpenError::Cpus)?;
        let (rings, pages, _) = rings(attr(options, start), pid, &cpus, options)?;
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
            reader_cpu: AtomicU64::new(0),
            cpus,
            followers: Vec::new(),
            sideband,
        })
    }

    /// Opens events for every thread of the running process `pid`, which
    /// follow every thread and child each starts after, into one ring per
    /// CPU. A thread needs an event on each CPU, so the events number its
    /// threads times its CPUs; past `budget` the attach is refused, rather
    /// than leaving threads unsampled.
    pub fn attach(pid: u32, options: &EventOptions, budget: usize) -> Result<Self, OpenError> {
        let cpus = online_cpus().map_err(OpenError::Cpus)?;
        Self::attach_on(pid, options, budget, &cpus)
    }

    /// Attaches as [`Self::attach`] does, with events only on `cpus`: a
    /// process kept to them needs no ring on any other.
    pub fn attach_on(
        pid: u32,
        options: &EventOptions,
        budget: usize,
        cpus: &[u32],
    ) -> Result<Self, OpenError> {
        raise_descriptor_limit();
        let mut attr = attr(options, 0);
        let sideband = sideband_attr(options, 0);
        let per_thread = cpus.len() * if sideband.is_some() { 2 } else { 1 };
        let mut opened = std::collections::BTreeSet::new();
        let mut owner = None;

        // A thread may start another before its own events open, so the
        // threads are listed again until no new one appears.
        loop {
            let threads = threads(pid).map_err(OpenError::NoProcess)?;
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
                    // The first thread's events own the rings.
                    match rings(attr, thread, cpus, options) {
                        Ok((rings, pages, watermark)) => {
                            attr.wakeup_watermark = watermark;
                            let sideband = match &sideband {
                                Some(sideband) => into_rings(sideband, thread, cpus, &rings)?,
                                None => Vec::new(),
                            };
                            owner = Some(Self {
                                rings,
                                pages,
                                buffered: collection_buffer(options),
                                reader_cpu: AtomicU64::new(0),
                                cpus: cpus.to_vec(),
                                followers: Vec::new(),
                                sideband,
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

    /// Opens `thread`'s events on each of `cpus`, and its `sideband`
    /// events if any, into the rings already open there. A thread that
    /// has exited has none.
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

    /// Data pages each ring has.
    pub const fn pages(&self) -> usize {
        self.pages
    }

    /// Runs the BPF program `program` as each event, and each event a
    /// thread inherits from one later, takes a sample, before the kernel
    /// writes it.
    pub fn run_program(&self, program: std::os::fd::BorrowedFd<'_>) -> std::io::Result<()> {
        for fd in self.rings.iter().map(sys::Ring::fd).chain(&self.followers) {
            sys::set_bpf(fd, program)?;
        }

        Ok(())
    }

    /// Opens a BPF output event on each CPU that writes to that CPU's
    /// ring, so that what a program writes through it lies beside the
    /// samples it is written for.
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

    /// Appends every ring's new records to `out`, a ring's whole records
    /// at a time, and returns how many bytes.
    pub fn drain(&self, out: &mut Vec<u8>) -> usize {
        let before = out.len();
        let mut buffered = self.buffered.as_ref().map(|buffer| {
            buffer
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        if let Some(buffer) = &mut buffered {
            out.append(buffer);
        }
        // Keep the queue locked through this drain so a worker cannot put
        // older records behind the records collected here, including at exit.
        for ring in &self.rings {
            ring.drain(out);
        }
        out.len() - before
    }

    /// Drains high-rate rings while `work` processes previous records. The
    /// queue holds at most 64 MiB plus one drain; if processing cannot keep
    /// up, kernel loss counters still account for every dropped sample.
    pub fn read_while<R>(&self, work: impl FnOnce() -> R) -> R {
        let Some(buffer) = &self.buffered else {
            return work();
        };
        let stop = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let reader = scope.spawn(|| {
                let began = sys::thread_cpu();
                while !stop.load(Ordering::Relaxed) {
                    let full = {
                        let mut buffer = buffer
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        let full = buffer.len() >= 64 * 1024 * 1024;
                        if !full {
                            for ring in &self.rings {
                                ring.drain(&mut buffer);
                            }
                        }
                        drop(buffer);
                        full
                    };
                    let elapsed = sys::thread_cpu().saturating_sub(began);
                    self.reader_cpu.store(
                        u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX),
                        Ordering::Relaxed,
                    );
                    if full {
                        // Full rings remain readable; polling them would spin.
                        std::thread::sleep(Duration::from_micros(100));
                    } else {
                        self.wait(Duration::from_micros(100));
                    }
                }
            });
            let guard = StopReader(&stop);
            let result = work();
            drop(guard);
            let _ = reader.join();
            result
        })
    }

    /// CPU time spent collecting on the high-rate reader.
    pub fn reader_cpu(&self) -> Duration {
        Duration::from_nanos(self.reader_cpu.load(Ordering::Relaxed))
    }

    /// How many times a ring's head could not be trusted, so that the ring
    /// skipped what it would have read.
    pub fn untrusted(&self) -> u64 {
        self.rings.iter().map(sys::Ring::untrusted).sum()
    }

    /// Waits until a ring is a quarter full, or `timeout`.
    pub fn wait(&self, timeout: Duration) {
        sys::poll(&self.rings, timeout);
    }

    /// What the events have counted and lost so far, exactly. An event
    /// counts each record it had no room for, whatever its kind, so the
    /// samples lost are the sampling events' losses, and the rest the
    /// sideband's. Events that sample nothing lose only other records.
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
        }
    }

    /// Starts or stops every event, and every event a thread inherited
    /// from one, counting and sampling.
    pub fn enable(&self, on: bool) {
        for fd in self
            .rings
            .iter()
            .map(sys::Ring::fd)
            .chain(&self.followers)
            .chain(&self.sideband)
        {
            let _ = sys::enable(fd, on);
        }
    }
}

struct StopReader<'a>(&'a AtomicBool);

impl Drop for StopReader<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

fn collection_buffer(options: &EventOptions) -> Option<Mutex<Vec<u8>>> {
    matches!(options.source, Source::Clock { period } if period <= Duration::from_millis(2))
        .then(|| Mutex::new(Vec::new()))
}

/// The flags of the records beside the samples: what each process maps and
/// names, its threads' lifetimes, and with `switches` their context
/// switches.
const fn sideband_flags(switches: bool) -> u64 {
    use sys::flag;
    flag::MMAP
        | flag::MMAP2
        | flag::COMM
        | flag::COMM_EXEC
        | flag::TASK
        | if switches { flag::CONTEXT_SWITCH } else { 0 }
}

/// The attributes of the events that own the rings: `options`' samples,
/// and `start`'s flags. Events that sample nothing write the records
/// beside samples themselves; otherwise [`sideband_attr`]'s do.
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
        sample_max_stack: 127,
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
            // PERIOD makes software-counted breakpoints sample every hit.
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

/// The attributes of the events that write every record but samples into
/// the rings, with `start`'s flags, where the rings' own events sample:
/// a dummy event, which counts what it loses as the sampling events count
/// theirs.
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

/// One event of `attr` for `pid` on each of `cpus`, each writing into the
/// ring of its CPU among `rings`.
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

/// One event of `attr` for `pid` on each of `cpus` with its ring, the rings'
/// data pages, and the watermark the events wake their reader at.
///
/// The kernel lets a user lock only so much memory for rings, which other
/// recordings may be using: rings that do not fit are halved, down to the
/// fewest that hold two samples. Each size opens its events afresh, as they
/// wake their reader a quarter of the way in.
fn rings(
    mut attr: sys::Attr,
    pid: i32,
    cpus: &[u32],
    options: &EventOptions,
) -> Result<(Vec<sys::Ring>, usize, u32), OpenError> {
    let fewest = min_pages(options.stack_bytes);
    let mut pages = options.ring_pages.next_power_of_two().max(fewest);

    loop {
        attr.wakeup_watermark = u32::try_from(pages * sys::page_size() / 4).unwrap_or(u32::MAX);
        let rings: std::io::Result<Vec<sys::Ring>> = open_each(&attr, pid, cpus)?
            .into_iter()
            .map(|fd| sys::Ring::map(fd, pages))
            .collect();

        match rings {
            Ok(rings) => return Ok((rings, pages, attr.wakeup_watermark)),
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

/// The threads of process `pid`.
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

/// Raises this process's limit on open descriptors as far as it may go,
/// as an attach holds one per thread per CPU.
fn raise_descriptor_limit() {
    use nix::sys::resource::{Resource, getrlimit, setrlimit};
    if let Ok((_, hard)) = getrlimit(Resource::RLIMIT_NOFILE) {
        let _ = setrlimit(Resource::RLIMIT_NOFILE, hard, hard);
    }
}

/// One event of `attr` for `pid` on each of `cpus`.
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

/// The CPUs `/sys` lists online, as `0-3,8` lists them.
fn online_cpus() -> std::io::Result<Vec<u32>> {
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

/// A running process sampled from outside, without stopping it.
#[derive(Debug)]
pub struct Attached {
    pub events: Events,
    pub process: ProcessWatch,
}

/// Says when one process exits: its identifier may name another after.
#[derive(Debug)]
pub struct ProcessWatch(OwnedFd);

impl ProcessWatch {
    /// Whether the process has exited.
    pub fn exited(&self) -> bool {
        sys::readable(&self.0)
    }
}

/// Samples the running process `pid` and everything it starts, within the
/// `budget` of events [`Events::attach`] takes.
pub fn attach(pid: u32, options: &EventOptions, budget: usize) -> Result<Attached, OpenError> {
    // Held first, so that the process the events name is the one watched.
    let process = super::native::pidfd_open(nix::unistd::Pid::from_raw(
        i32::try_from(pid).unwrap_or(i32::MAX),
    ))
    .map_err(|error| OpenError::NoProcess(error.into()))?;
    let events = Events::attach(pid, options, budget)?;

    Ok(Attached {
        events,
        process: ProcessWatch(process),
    })
}

/// The load bias of the program process `pid` runs, from its mapping of
/// the start of its file.
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

/// A program started for sampling.
#[derive(Debug)]
pub struct Launched {
    pub pid: u32,
    pub events: Events,

    /// Ends with the program's exit status, once it exits.
    pub exit: JoinHandle<std::io::Result<ExitStatus>>,

    /// Kills the program it was opened for and no later process given its
    /// identifier.
    pub killer: Killer,

    /// Says the program executed; it closes without a word when it failed
    /// to, and `exit` holds why.
    pub executed: std::sync::mpsc::Receiver<()>,
}

/// Kills one launched program.
#[derive(Debug)]
pub struct Killer(OwnedFd);

impl Killer {
    /// Kills the program, unless it has ended already.
    pub fn kill(&self) {
        // A program that has ended already is no failure.
        let _ = super::native::pidfd_send_signal(&self.0, super::signals::Signal::SIGKILL);
    }
}

/// How to start the program.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LaunchOptions {
    /// Runs it at the addresses its image names, as breakpoint sources
    /// need to know where code is before it runs.
    pub no_address_randomization: bool,
}

/// Starts `command` with its events open before it executes, so the first
/// instruction of the program is sampled and nothing before it is.
///
/// The child waits between fork and exec until its events are open. A
/// thread of its own spawns it, because spawning returns only after the
/// exec, and stays to wait for its exit: the child dies with that thread.
/// `held` runs while it waits, given its pid and its events, so that what
/// `held` starts or attaches to the events sees it from its first
/// instruction too; an error from `held` ends the child before it
/// executes.
pub fn launch(
    mut command: Command,
    options: &EventOptions,
    launch: LaunchOptions,
    held: impl FnOnce(u32, &Events) -> Result<(), String>,
) -> Result<Launched, LaunchError> {
    let (pid_read, pid_write) = pipe()?;
    let (go_read, go_write) = pipe()?;
    gate(&mut command, [&pid_write, &go_read, &go_write], launch);
    let (executing, executed) = std::sync::mpsc::channel();
    let spawner = std::thread::Builder::new()
        .name("microscope-record-child".into())
        .spawn(move || {
            // Spawning returns once the program executes, or has failed to.
            let spawned = command.spawn();

            // The child holds its own copies until it executes.
            drop((pid_write, go_read));
            let child = spawned?;
            let _ = executing.send(());

            // A task snapshot traces the child from this process, which any
            // wait for it here would take the stops of: wait for its exit
            // alone, then reap it once no snapshot holds it (see `reaped`).
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
        // The fork failed, or the child failed before it reached the gate.
        let error = match spawner.join() {
            Ok(Err(error)) => error,
            _ => std::io::Error::other("the program did not start"),
        };

        return Err(LaunchError::Spawn(error));
    }

    let pid = u32::from_le_bytes(pid);
    record!("perf launch: {pid} waits at its gate");

    // Until the gate opens the child cannot exit.
    super::reaped::REAPED.watch(pid);

    // Until the gate opens the child can neither execute nor exit, so the
    // descriptor names it and no other process.
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

    let events = match Events::open(pid, options) {
        Ok(events) => events,
        Err(error) => {
            record!("perf launch: closing {pid}'s gate unopened: {error}");

            // Closing the gate unopened ends the child before it executes.
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

    write_all(&go_write, &[1])?;
    record!("perf launch: {pid} opened its gate");

    Ok(Launched {
        pid,
        events,
        exit: spawner,
        killer,
        executed,
    })
}

/// How long after finding its child reaped a launch waits for the thread
/// that reaped it to leave its status.
const REAPED_PATIENCE: Duration = Duration::from_secs(5);

/// Reaps `child`, or takes its status from the thread of this process that
/// reaped it first.
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
    launch: LaunchOptions,
) {
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
            ..Default::default()
        },
    );
}

/// The vDSO's image, read from this process's own memory, and the address
/// it is linked at. The kernel maps the one image it holds into every
/// 64-bit process, so this is every recorded program's vDSO too.
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

/// Whether an executable mapping is the vDSO's.
pub fn is_vdso(mmap: &decode::Mmap<'_>) -> bool {
    mmap.executable && mmap.path == super::vdso::VDSO_NAME.as_bytes()
}

/// A file to read an executable mapping's image from, and the image's load
/// bias, if one is still the file mapped.
///
/// The file is the mapped path, or the same path in the process's own root
/// where the process sees other mounts, as in a container. A file deleted
/// since is still the process's: its mapping's link in `map_files`, which
/// needs privileges, or for the program itself, its `exe`.
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

/// One line of a process's memory map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mapped {
    pub start: u64,
    pub end: u64,
    pub file_offset: u64,
    pub inode: u64,
    pub executable: bool,

    /// Whether the process may write it, as a JIT compiler's code.
    pub writable: bool,
    pub path: Vec<u8>,
}

/// The executable mappings of process `pid` now, which the records of an
/// attach began after. Anonymous ones are named `//anon`, as perf's
/// records name them.
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

        // Anonymous code, as perf's records name it.
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

/// Whether `mapping` is anonymous code, which no file backs: a JIT
/// compiler's, or a copy a runtime made of a module's code.
pub fn is_anonymous(mapping: &Mapped) -> bool {
    mapping.inode == 0 && (mapping.path == b"//anon" || mapping.path.starts_with(b"[anon:"))
}

/// The range of one of `sources`' code that process `pid`'s anonymous
/// code at `range` begins with a copy of, if any, read from the running
/// process (see [`crate::unwind::code_copies`]).
pub fn copy_of<K: Copy>(
    pid: u32,
    range: &std::ops::Range<u64>,
    sources: &[crate::unwind::code_copies::Source<'_, K>],
) -> Option<crate::unwind::code_copies::Proven<K, ()>> {
    use crate::unwind::code_copies::{PROBE, compare_bytes, prove};

    /// How much of a copy is compared at once.
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

/// The ranges of the image `info` describes, loaded as `loaded` over
/// `mapped`, that a runtime it carries says process `pid` keeps its own
/// code in, where no symbol bounds it, as V8's builtins in a stripped
/// libnode, where a copy of them may begin.
pub fn stored_code(
    pid: u32,
    info: &crate::debug_info::DebugInfo,
    loaded: crate::LoadedModule,
    mapped: std::ops::Range<u64>,
) -> Vec<(crate::ImageAddress, crate::ImageAddress)> {
    stored_code_of(&code_keepers(info), pid, loaded, mapped)
}

/// The runtimes the image `info` describes carries that may keep code of
/// their own in a process, for [`stored_code_of`]. Detecting them reads
/// the image's symbols, so a caller that asks of one image often keeps
/// them.
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

/// [`stored_code`], with the runtimes [`code_keepers`] found in the image.
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

/// Reads process `pid`'s memory at `address` into `buffer`, from outside
/// it: how many bytes it read, which a mapping's end may cut short.
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

/// Fills each of `reads`' buffers from process `pid`'s memory at its
/// address, from outside it, in as few system calls as it can: whether
/// each was filled whole.
///
/// The kernel reads the buffers in order and stops at the first it cannot
/// fill whole, filling none of it (`process_vm_readv(2)`: it never splits
/// one), so each call fills a run of them; the one that ended the run is
/// not filled, and the rest are read on from the one after it. A failure
/// other than a fault, as of a process that is gone, fills none.
pub fn read_memory_each(pid: u32, reads: &mut [(crate::VirtualAddress, &mut [u8])]) -> Vec<bool> {
    // The most buffers one call takes (`UIO_MAXIOV`).
    const MOST: usize = 1024;
    let mut filled = vec![false; reads.len()];
    let mut next = 0;
    while next < reads.len() {
        let end = reads.len().min(next + MOST);
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
        filled[next..next + whole].fill(true);
        next += whole;
        // The buffer that ended the run, unless the call took all it was
        // given.
        if next < end {
            next += 1;
        }
    }
    filled
}

/// Writes `bytes` to process `pid`'s memory at `address`, from outside it.
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

/// Whether the kernel keeps time by the time-stamp counter, which it does
/// only where the counter is invariant and synchronized across CPUs, so
/// that every thread's counter is one clock.
pub fn tsc_is_the_clock() -> bool {
    std::fs::read_to_string("/sys/devices/system/clocksource/clocksource0/current_clocksource")
        .is_ok_and(|source| source.trim() == "tsc")
}

/// The time-stamp counter and `CLOCK_MONOTONIC` at one moment: the
/// closest of a few readings of the counter between two of the clock.
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

    /// The end of a readable mapping of this process with nothing mapped
    /// just past it.
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

    /// Errors print with their chain of causes, so one whose message names
    /// its cause must not also report it as its source.
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

    /// A ring never drained fills at once and loses the rest. A program
    /// that switches threads far more often than its CPU time makes
    /// samples loses many more records than it could samples, and the
    /// samples counted lost are only those its CPU time allowed: about the
    /// 50 ms it spins in its own code once its threads stop switching.
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
            LaunchOptions::default(),
            |_, _| Ok(()),
        )
        .expect("launch the program");
        let status = launched
            .exit
            .join()
            .expect("the launch thread ends")
            .expect("the program exits");
        assert!(status.success(), "{status}");

        let counts = launched.events.counts();
        // Each event's count is sampled once a whole period of it passes,
        // so each CPU's event may hold one period's part over.
        let possible = counts.counted / PERIOD_NS + launched.events.cpus.len() as u64;
        assert!(
            (1..=possible).contains(&counts.lost_samples),
            "lost {counts:?}, but {possible} samples were possible"
        );
        // Each of 20,000 rounds switches each thread out and in.
        assert!(
            counts.lost_records > 10 * possible,
            "lost {counts:?}, though the ring was never read"
        );
    }

    #[test]
    fn each_read_is_filled_whole_or_not_at_all_and_the_rest_read_on() {
        let pid = std::process::id();
        let end = mapping_end();
        let source: Vec<[u8; 8]> = (0..1500u64).map(u64::to_le_bytes).collect();
        let at = |index: usize| crate::VirtualAddress::new(source[index].as_ptr() as u64);

        // Index: what it reads, and whether it can be read whole.
        let mut plan: Vec<(crate::VirtualAddress, usize, bool)> = (0..source.len())
            .map(|index| (at(index), 8, true))
            .collect();
        // Cut short by the mapping's end, then nothing mapped, each
        // followed by readable ones; and the last of the first call's
        // buffers, and the first of the second's.
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
