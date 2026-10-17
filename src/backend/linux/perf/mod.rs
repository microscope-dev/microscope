//! Sampling processes with perf events, from outside them: no ptrace and
//! no code in the program. One event per CPU follows a process and every
//! thread and child it starts, and writes samples, mappings, and thread
//! lifetimes to that CPU's ring.

pub mod decode;
pub mod jit;
pub mod sys;

use std::os::fd::{AsRawFd, OwnedFd};
use std::path::PathBuf;
use std::process::{Command, ExitStatus};
use std::thread::JoinHandle;
use std::time::Duration;

use nix::libc;

use super::modules::{ModuleMapping, identify_mapped_module};

/// What makes a sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Every `period` of CPU time the program's threads spend in user code.
    Clock { period: Duration },

    /// Every execution of the instruction at `address`.
    Breakpoint { address: u64 },

    /// No samples: only what the program maps, and its threads.
    Mappings,
}

/// How events sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventOptions {
    pub source: Source,

    /// Bytes of stack each sample copies, a multiple of eight.
    pub stack_bytes: u32,

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
        "the kernel does not let this user sample the program (perf_event_paranoid is {paranoid}): {source}"
    )]
    Denied {
        paranoid: String,
        source: std::io::Error,
    },

    #[error("opening a perf event on CPU {cpu} failed: {source}")]
    Event { cpu: u32, source: std::io::Error },

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
        "sampling {threads} threads on {cpus} CPUs takes {} events, more than the budget of {budget}",
        threads * cpus
    )]
    TooManyEvents {
        threads: usize,
        cpus: usize,
        budget: usize,
    },
}

/// One event per online CPU, each with its ring.
#[derive(Debug)]
pub struct Events {
    rings: Vec<sys::Ring>,
    pages: usize,

    /// The CPU of each ring, in their order.
    cpus: Vec<u32>,

    /// Events that write to the rings of others on their CPUs.
    followers: Vec<OwnedFd>,
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
        let attr = attr(options, sys::flag::DISABLED | sys::flag::ENABLE_ON_EXEC);
        let pid = i32::try_from(pid).map_err(|_| OpenError::Cpus(std::io::Error::other("pid")))?;
        let cpus = online_cpus().map_err(OpenError::Cpus)?;
        let (rings, pages, _) = rings(attr, pid, &cpus, options)?;
        record!(
            "perf events for {pid}: {} CPUs, {pages} pages each",
            rings.len()
        );
        Ok(Self {
            rings,
            pages,
            cpus,
            followers: Vec::new(),
        })
    }

    /// Opens events for every thread of the running process `pid`, which
    /// follow every thread and child each starts after, into one ring per
    /// CPU. A thread needs an event on each CPU, so the events number its
    /// threads times its CPUs; past `budget` the attach is refused, rather
    /// than leaving threads unsampled.
    pub fn attach(pid: u32, options: &EventOptions, budget: usize) -> Result<Self, OpenError> {
        let cpus = online_cpus().map_err(OpenError::Cpus)?;
        raise_descriptor_limit();
        let mut attr = attr(options, 0);
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

            if (opened.len() + new.len()) * cpus.len() > budget {
                return Err(OpenError::TooManyEvents {
                    threads: opened.len() + new.len(),
                    cpus: cpus.len(),
                    budget,
                });
            }

            for tid in new {
                opened.insert(tid);
                let thread = i32::try_from(tid).unwrap_or(i32::MAX);
                let Some(events) = owner.as_mut() else {
                    // The first thread's events own the rings.
                    match rings(attr, thread, &cpus, options) {
                        Ok((rings, pages, watermark)) => {
                            attr.wakeup_watermark = watermark;
                            owner = Some(Self {
                                rings,
                                pages,
                                cpus: cpus.clone(),
                                followers: Vec::new(),
                            });
                        }

                        Err(OpenError::Event { source, .. })
                            if source.raw_os_error() == Some(libc::ESRCH) => {}
                        Err(error) => return Err(error),
                    }

                    continue;
                };

                events.follow(&attr, thread, &cpus)?;
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

    /// Opens `thread`'s events on each of `cpus`, into the rings already
    /// open there. A thread that has exited has none.
    fn follow(&mut self, attr: &sys::Attr, thread: i32, cpus: &[u32]) -> Result<(), OpenError> {
        for (ring, &cpu) in self.rings.iter().zip(cpus) {
            let fd = match sys::open(attr, thread, i32::try_from(cpu).unwrap_or(i32::MAX)) {
                Ok(fd) => fd,
                Err(error) if error.raw_os_error() == Some(libc::ESRCH) => return Ok(()),
                Err(source) => return Err(OpenError::Event { cpu, source }),
            };

            sys::set_output(&fd, ring.fd()).map_err(|source| OpenError::Event { cpu, source })?;
            self.followers.push(fd);
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
                let failed = |source| OpenError::Event { cpu, source };
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
    pub fn drain(&mut self, out: &mut Vec<u8>) -> usize {
        self.rings.iter_mut().map(|ring| ring.drain(out)).sum()
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

    /// What the events have counted so far, in their source's units, how
    /// many samples they have lost, exactly, and how many times a ring's
    /// head could not be trusted.
    pub fn counts(&self) -> (u64, u64, u64) {
        let (counted, lost) = self
            .rings
            .iter()
            .map(sys::Ring::fd)
            .chain(&self.followers)
            .filter_map(|fd| sys::counts(fd).ok())
            .fold((0, 0), |(count, lost), (more, also)| {
                (count + more, lost + also)
            });
        (counted, lost, self.untrusted())
    }

    /// Starts or stops every event, and every event a thread inherited
    /// from one, counting and sampling.
    pub fn enable(&self, on: bool) {
        for fd in self.rings.iter().map(sys::Ring::fd).chain(&self.followers) {
            let _ = sys::enable(fd, on);
        }
    }
}

/// The attributes of every event: `options`' samples, with every record
/// of mappings and threads beside them, and `start`'s flags.
fn attr(options: &EventOptions, start: u64) -> sys::Attr {
    use sys::flag;
    let mut attr = sys::Attr {
        size: sys::ATTR_SIZE,
        sample_type: decode::SAMPLE_TYPE,
        flags: start
            | flag::INHERIT
            | flag::EXCLUDE_KERNEL
            | flag::EXCLUDE_HV
            | flag::MMAP
            | flag::MMAP2
            | flag::COMM
            | flag::COMM_EXEC
            | flag::TASK
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

    if options.switches {
        attr.flags |= flag::CONTEXT_SWITCH;
    }

    match options.source {
        Source::Clock { period } => {
            attr.kind = sys::TYPE_SOFTWARE;
            attr.config = sys::COUNT_SW_CPU_CLOCK;
            attr.sample_period = u64::try_from(period.as_nanos()).unwrap_or(u64::MAX).max(1);
        }
        Source::Breakpoint { address } => {
            attr.kind = sys::TYPE_BREAKPOINT;
            attr.bp_type = sys::BREAKPOINT_EXECUTE;
            attr.bp_addr = address;
            attr.bp_len = 8;
            attr.sample_period = 1;
        }
        Source::Mappings => {
            attr.kind = sys::TYPE_SOFTWARE;
            attr.config = sys::COUNT_SW_DUMMY;
        }
    }

    attr
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
            sys::open(attr, pid, i32::try_from(cpu).unwrap_or(i32::MAX)).map_err(|source| {
                match source.raw_os_error() {
                    Some(libc::EACCES | libc::EPERM) => OpenError::Denied {
                        paranoid: std::fs::read_to_string("/proc/sys/kernel/perf_event_paranoid")
                            .map_or_else(|_| "unknown".into(), |text| text.trim().to_owned()),
                        source,
                    },
                    _ => OpenError::Event { cpu, source },
                }
            })
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
                path: path.to_vec(),
            });
        }
    }

    Ok(mappings)
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
