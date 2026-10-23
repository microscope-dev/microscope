//! Go's runtime: goroutines are its tasks, listed in `runtime.allgs`, and a
//! thread's goroutine is the g in its thread-local storage.
//!
//! The model checks every g it follows against `allgs`, so a corrupted
//! pointer is reported, never read as a goroutine.

mod exceptions;
mod layout;
mod types;

use std::sync::{Arc, OnceLock};

use layout::{Goroutines, Labels, Layout, Missing, Threads};

use super::{
    CodeAddress, Crossing, DynamicValue, Partial, RuntimeException, RuntimeHook, RuntimeImage,
    RuntimeModel, RuntimeSignals, RuntimeStop, RuntimeTask, StartedTask, StoredValue, TaskContext,
    TaskLabels, TaskPage, TaskRef, TaskStarter, ThreadActivity, Unbound,
};

use crate::unwind::{
    DwarfRegister, R12, RAX, RBP, RDI, RETURN_ADDRESS as RIP, RSI, RSP, RegisterFile,
};

use crate::{AddressRange, ImageAddress, StackSegment, TaskState, ThreadId, VirtualAddress};

/// The release the contract was checked against. Another release is read
/// the same way wherever its debug information binds, and says it is
/// unverified.
const VERIFIED: (u64, u64) = (1, 27);

/// What Go calls its tasks.
pub(super) const TASK_NOUN: (&str, &str) = ("goroutine", "goroutines");

pub(super) use exceptions::EXCEPTION_FILTERS;

/// Linux's signal for urgent socket data, which the runtime preempts with.
const SIGURG: i32 = 23;

/// Linux's signals for faults, which the runtime turns into panics.
const SIGBUS: i32 = 7;
const SIGFPE: i32 = 8;
const SIGSEGV: i32 = 11;

/// How the runtime uses signals. A fault in Go code becomes a panic, and
/// the runtime preempts with SIGURG, which may arrive whenever it still
/// wants to preempt.
const SIGNALS: RuntimeSignals = RuntimeSignals {
    deferrable: &[SIGURG],
    handled: &[SIGBUS, SIGFPE, SIGSEGV],
};

/// Why goroutines cannot be read from a program without debug information.
const STRIPPED: &str = "the program has no debug information describing Go's runtime";

/// The first release whose runtime the model can read at all.
const OLDEST: (u64, u64) = (1, 20);

/// The most goroutines one list reads, so a corrupted `allglen` cannot
/// make the debugger read without end.
const MAX_GOROUTINES: u64 = 1 << 24;

/// How many entries of `allgs` a page of tasks scans for each task it may
/// hold, and at least, whatever its limit.
const SCAN_PER_TASK: u64 = 4;
const MIN_SCAN: u64 = 256;

/// The most profiler labels read from one goroutine.
const MAX_LABELS: u64 = 64;

/// The Go runtime in an image whose units Go compiled.
pub fn detect(
    image: Arc<dyn RuntimeImage + Send + Sync>,
) -> Option<Result<Arc<dyn RuntimeModel>, Unbound>> {
    let Some(version) = image
        .producers()
        .iter()
        .find_map(|producer| version(producer))
    else {
        // A program whose debug information was stripped still names the
        // runtime's code in Go's own function table, but nothing describes
        // the runtime's data. It still preempts with signals.
        return image.has_function("runtime.goexit").then(|| {
            Err(Unbound {
                reason: STRIPPED.into(),
                deferrable: SIGNALS.deferrable,
            })
        });
    };

    Some(
        GoRuntime::bind(image, version)
            .map(|model| Arc::new(model) as Arc<dyn RuntimeModel>)
            .map_err(|reason| Unbound {
                reason,
                deferrable: SIGNALS.deferrable,
            }),
    )
}

/// The release in a Go producer, such as `Go cmd/compile go1.27.1; regabi`.
fn version(producer: &str) -> Option<(u64, u64, Arc<str>)> {
    let release = producer.strip_prefix("Go cmd/compile go")?;
    let release = release.split([';', ' ']).next()?;
    let mut parts = release.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts
        .next()?
        .split(|character: char| !character.is_ascii_digit())
        .next()?
        .parse()
        .ok()?;
    Some((major, minor, format!("go{release}").into()))
}

#[derive(Debug)]
struct GoRuntime {
    image: Arc<dyn RuntimeImage + Send + Sync>,
    layout: Layout,

    /// Why every result may be wrong: a release the contract was not
    /// checked against.
    unverified: Option<Arc<str>>,

    /// Where the first thread runs before its thread pointer is set. A
    /// thread `clone` makes starts with the thread pointer of its own m,
    /// whose g slot is empty until the thread sets it.
    starting: Vec<AddressRange<ImageAddress>>,

    /// The functions whose goroutines are the runtime's own work, with
    /// those that are the program's despite being in the runtime.
    internal: Internal,

    /// The runtime's names for goroutine statuses and wait reasons, which
    /// are static data read at the first stop that needs them.
    names: OnceLock<Names>,

    /// The functions that report panics and fatal errors, and how what
    /// they report is laid out.
    hooks: exceptions::Hooks,
    exceptions: Result<exceptions::Layout, Missing>,

    /// How interface values hold their dynamic values.
    interfaces: Result<types::Interfaces, Missing>,

    /// `runtime.copystack(gp *g, newsize uintptr)`, which moves a
    /// goroutine's stack to grow or shrink it.
    copystack: Option<ImageAddress>,

    /// `runtime.newproc1`, which makes the goroutine a `go` statement
    /// starts and returns it.
    newproc1: Option<ImageAddress>,

    /// The functions cgo calls between Go and C through, each with the
    /// register that holds what it calls: `runtime.cgocall(fn, arg)` calls
    /// C, and `crosscall2(fn, a, n, ctxt)` the Go that C calls.
    call_outs: Vec<(ImageAddress, DwarfRegister, &'static str)>,
}

impl GoRuntime {
    fn bind(
        image: Arc<dyn RuntimeImage + Send + Sync>,
        (major, minor, release): (u64, u64, Arc<str>),
    ) -> Result<Self, Arc<str>> {
        if (major, minor) < OLDEST {
            return Err(format!("{release}'s runtime is too old to read").into());
        }

        let unverified = ((major, minor) != VERIFIED).then(|| {
            format!(
                "{release} is unverified; its runtime is read as go{}.{}'s",
                VERIFIED.0, VERIFIED.1
            )
            .into()
        });

        // An assembly function's symbol carries its ABI.
        let code = |name: &str| {
            let symbol = image
                .symbol(name)
                .or_else(|| image.symbol(&format!("{name}.abi0")))?;
            Some(AddressRange {
                start: symbol.address,
                end: ImageAddress::new(symbol.address.get().checked_add(symbol.size?)?),
            })
        };

        let starting = code("runtime.settls").into_iter().collect();

        Ok(Self {
            layout: Layout::bind(image.as_ref()),
            internal: Internal::bind(image.as_ref()),
            hooks: exceptions::Hooks::bind(image.as_ref()),
            exceptions: exceptions::Layout::bind(image.as_ref()),
            interfaces: types::Interfaces::bind(image.as_ref()),
            copystack: layout::symbol(image.as_ref(), "runtime.copystack").ok(),
            newproc1: layout::symbol(image.as_ref(), "runtime.newproc1").ok(),
            call_outs: [("runtime.cgocall", RAX, "rax"), ("crosscall2", RDI, "rdi")]
                .into_iter()
                .filter_map(|(name, register, register_name)| {
                    Some((code(name)?.start, register, register_name))
                })
                .collect(),
            image,
            unverified,
            starting,
            names: OnceLock::new(),
        })
    }

    fn partial<T>(&self, value: T) -> Partial<T> {
        Partial {
            value,
            gaps: self.unverified.iter().cloned().collect(),
        }
    }

    fn goroutines(&self) -> Result<&Goroutines, Missing> {
        self.layout.goroutines.as_ref().map_err(Arc::clone)
    }

    fn threads(&self) -> Result<&Threads, Missing> {
        self.layout.threads.as_ref().map_err(Arc::clone)
    }

    /// The g pointers in `allgs`, as many as `allglen` publishes.
    fn allgs(&self, stop: &dyn RuntimeStop) -> Result<Vec<u64>, Arc<str>> {
        self.allgs_from(stop, 0, MAX_GOROUTINES)
    }

    /// At most `count` of the g pointers in `allgs` from index `start`,
    /// reading no more of the list than that.
    fn allgs_from(
        &self,
        stop: &dyn RuntimeStop,
        start: u64,
        count: u64,
    ) -> Result<Vec<u64>, Arc<str>> {
        let layout = self.goroutines()?;
        let bias = stop.load_bias();
        let at = |address: ImageAddress| bias.wrapping_load(address);
        let length = word(stop, at(layout.allglen)).ok_or("runtime.allglen is unreadable")?;
        let array = word(stop, at(layout.allgs)).ok_or("runtime.allgs is unreadable")?;
        let capacity = word(
            stop,
            VirtualAddress::new(at(layout.allgs).get().wrapping_add(8)),
        )
        .ok_or("runtime.allgs is unreadable")?;
        if length > capacity || length > MAX_GOROUTINES {
            return Err(
                format!("runtime.allglen is {length}, beyond what runtime.allgs holds").into(),
            );
        }

        let end = start.saturating_add(count).min(length);
        let Some(read) = end.checked_sub(start).filter(|read| *read > 0) else {
            return Ok(Vec::new());
        };

        let mut bytes = vec![0; usize::try_from(read * 8).map_err(|_| "allgs is too large")?];
        if !stop.read(
            VirtualAddress::new(array.wrapping_add(start * 8)),
            &mut bytes,
        ) {
            return Err(format!("runtime.allgs at {array:#x} is unreadable").into());
        }

        Ok(bytes
            .as_chunks::<8>()
            .0
            .iter()
            .map(|chunk| u64::from_le_bytes(*chunk))
            .collect())
    }

    /// Reads one goroutine, or `None` for one that is dead.
    fn goroutine(
        &self,
        stop: &dyn RuntimeStop,
        names: &Names,
        g: u64,
    ) -> Result<Option<RuntimeTask>, Arc<str>> {
        let layout = self.goroutines()?;
        let unreadable = || Arc::<str>::from(format!("goroutine at {g:#x} is unreadable"));
        let field = |offset: u64, size: usize| {
            read_unsigned(stop, VirtualAddress::new(g.wrapping_add(offset)), size)
                .ok_or_else(unreadable)
        };

        let word = |offset| field(offset, 8);
        let status = field(layout.status, 4)?;
        let statuses = &layout.statuses;
        let base = status & !statuses.scan;
        if base == statuses.dead || Some(base) == statuses.dead_extra {
            return Ok(None);
        }

        let number = word(layout.goid)?;

        // A goroutine is on a thread while its m runs it, or runs the
        // runtime's code for it, as when it is being parked or scheduled.
        let m = word(layout.m)?;
        let thread = if m == 0 {
            None
        } else {
            let m_word = |offset: u64| {
                read_unsigned(stop, VirtualAddress::new(m.wrapping_add(offset)), 8).ok_or_else(
                    || Arc::<str>::from(format!("the thread of goroutine {number} is unreadable")),
                )
            };

            let procid = m_word(layout.m_procid)?;
            (procid != 0 && m_word(layout.m_curg)? == g).then(|| ThreadId::new(procid))
        };

        let state = if base == statuses.running || base == statuses.syscall {
            TaskState::Running
        } else if base == statuses.runnable || base == statuses.preempted {
            TaskState::Runnable
        } else if base == statuses.waiting
            || base == statuses.copystack
            || Some(base) == statuses.leaked
        {
            TaskState::Blocked
        } else if base == statuses.idle {
            TaskState::Unknown("the goroutine is being created".into())
        } else {
            TaskState::Unknown(format!("the goroutine's status {status:#x} is unknown").into())
        };

        // A waiting goroutine is described by why it waits, as the
        // runtime's own tracebacks do; any other by its status.
        let wait_reason = field(layout.wait_reason, 1)?;
        let detail = if base == statuses.waiting && wait_reason != 0 {
            names
                .wait_reasons
                .get(usize::try_from(wait_reason).unwrap_or(usize::MAX))
                .cloned()
                .flatten()
        } else {
            names
                .statuses
                .get(usize::try_from(base).unwrap_or(usize::MAX))
                .cloned()
                .flatten()
        };

        let address = |value: u64| (value != 0).then(|| VirtualAddress::new(value));
        let on_thread = base == statuses.running || base == statuses.syscall;
        let entry = word(layout.startpc)?;

        // A parked goroutine resumes after the call that saved its
        // registers, except one that never ran, which begins at its entry.
        let resume = word(layout.sched_pc)?;
        let resume = address(resume).map(|address| CodeAddress {
            address,
            after_call: resume != entry,
        });

        // A goroutine is created by a call to the runtime. The main
        // goroutine is the runtime's start-up's, which Go's traceback does
        // not name.
        let creation = address(word(layout.gopc)?)
            .filter(|_| number != 1)
            .map(|address| CodeAddress {
                address,
                after_call: true,
            });

        Ok(Some(RuntimeTask {
            number,
            locator: g,
            state,
            detail,
            thread,
            resume: if on_thread { None } else { resume },
            creation,
            spawned: None,
            entry: address(entry),
            parent: layout
                .parent_goid
                .map(word)
                .transpose()?
                .filter(|parent| *parent != 0),
            internal: self.internal.is_internal(
                self.image.as_ref(),
                stop.load_bias().wrapping_unload(VirtualAddress::new(entry)),
            ),
            labels: Vec::new(),
        }))
    }

    fn names(&self, stop: &dyn RuntimeStop) -> &Names {
        self.names.get_or_init(|| Names::read(self, stop))
    }
}

impl RuntimeModel for GoRuntime {
    /// A page scans at most a fixed multiple of its limit of `allgs`, so
    /// dead goroutines and the runtime's own, which it leaves out, never
    /// make one page read the whole list.
    fn tasks(
        &self,
        stop: &dyn RuntimeStop,
        start: u64,
        limit: usize,
        program_only: bool,
    ) -> Partial<TaskPage> {
        let mut page = self.partial(TaskPage {
            tasks: Vec::new(),
            next: None,
        });
        let scanned = u64::try_from(limit)
            .unwrap_or(u64::MAX)
            .saturating_mul(SCAN_PER_TASK)
            .max(MIN_SCAN);

        // One more than the scan shows whether any are left.
        let allgs = match self.allgs_from(stop, start, scanned.saturating_add(1)) {
            Ok(allgs) => allgs,
            Err(reason) => {
                page.gaps.push(reason);
                return page;
            }
        };

        let names = self.names(stop);

        // Without `g.labels`, every goroutine's labels are unknown, which
        // the page says once.
        let labels = self.layout.labels.as_ref();
        if let Err(reason) = labels {
            page.gaps
                .push(format!("profiler labels are not read: {reason}").into());
        }

        for (index, &g) in (start..).zip(&allgs) {
            if page.value.tasks.len() == limit || index - start == scanned {
                page.value.next = Some(index);
                break;
            }

            match self.goroutine(stop, names, g) {
                Ok(Some(task)) if program_only && task.internal => {}
                Ok(Some(mut task)) => {
                    if let Ok(layout) = labels {
                        match read_labels(stop, layout, g) {
                            Ok(read) => task.labels = read,
                            Err(reason) => page.gaps.push(
                                format!("goroutine {}'s labels: {reason}", task.number).into(),
                            ),
                        }
                    }

                    page.value.tasks.push(task);
                }
                Ok(None) => {}
                Err(reason) => page.gaps.push(reason),
            }
        }

        page
    }

    fn thread_activity(&self, stop: &dyn RuntimeStop, thread: ThreadId) -> ThreadActivity {
        match self.activity(stop, thread) {
            Ok(activity) => activity,
            Err(reason) => ThreadActivity::Unknown(reason),
        }
    }

    fn task_context(
        &self,
        stop: &dyn RuntimeStop,
        task: TaskRef,
    ) -> Result<Option<TaskContext>, Arc<str>> {
        let layout = self.goroutines()?;
        let number = task.number;
        let Some((g, task)) = self.find(stop, task)? else {
            return Ok(None);
        };

        // A goroutine that runs, or makes a system call, has its registers
        // on its thread; any other saved them when it last stopped running.
        if task.state == TaskState::Running {
            return task
                .thread
                .map(|thread| Some(TaskContext::OnThread(thread)))
                .ok_or_else(|| format!("the thread running goroutine {number} is unknown").into());
        }

        let word = |offset: u64| {
            read_unsigned(stop, VirtualAddress::new(g.wrapping_add(offset)), 8).ok_or_else(|| {
                Arc::<str>::from(format!(
                    "goroutine {number}'s saved registers are unreadable"
                ))
            })
        };

        let (pc, sp, bp) = (
            word(layout.sched_pc)?,
            word(layout.sched_sp)?,
            word(layout.sched_bp)?,
        );
        if pc == 0 || sp == 0 {
            return Err(format!("goroutine {number} has no saved registers").into());
        }

        // A parked goroutine's frames are on its own stack, which a
        // corrupted record would have the walk read anywhere.
        let stack = self.stack(stop, g)?;
        if !stack.contains(&sp) {
            return Err(format!(
                "goroutine {number}'s saved stack pointer {sp:#x} is outside its stack \
                 {:#x}..{:#x}",
                stack.start, stack.end
            )
            .into());
        }

        let mut registers = RegisterFile::new([(RIP, pc), (RSP, sp)]);

        // A saved frame pointer of zero is unknown, not a frame at zero.
        if bp != 0 {
            registers.set(RBP, bp);
        }

        Ok(Some(TaskContext::Saved {
            registers,
            after_call: task.resume.is_none_or(|resume| resume.after_call),
        }))
    }

    fn thread_stacks(
        &self,
        stop: &dyn RuntimeStop,
        thread: ThreadId,
    ) -> Result<Vec<(std::ops::Range<u64>, StackSegment)>, Arc<str>> {
        let Some(gs) = self.thread_gs(stop, thread)? else {
            return Ok(Vec::new());
        };

        let mut stacks = Vec::new();
        for (g, segment) in [
            (gs.curg, StackSegment::Task),
            (gs.g0, StackSegment::System),
            (gs.gsignal, StackSegment::Signal),
        ] {
            if g == 0 {
                continue;
            }

            let stack = self.stack(stop, g)?;
            if !stack.is_empty() {
                stacks.push((stack, segment));
            }
        }

        Ok(stacks)
    }

    fn task_noun(&self) -> &'static str {
        TASK_NOUN.0
    }

    fn stack_mover(&self) -> Option<ImageAddress> {
        self.copystack
    }

    /// `copystack` takes the goroutine in rax, which may be any goroutine:
    /// its own as it grows, or another the collector shrinks.
    fn moving_task(
        &self,
        stop: &dyn RuntimeStop,
        registers: &RegisterFile,
    ) -> Result<u64, Arc<str>> {
        let g = registers
            .get(RAX)
            .ok_or("the goroutine copystack moves is unavailable")?;
        if !self.allgs(stop)?.contains(&g) {
            return Err(format!("copystack moves {g:#x}, which is no goroutine").into());
        }

        let names = self.names(stop);
        self.goroutine(stop, names, g)?
            .map(|task| task.number)
            .ok_or_else(|| format!("copystack moves {g:#x}, which runs no goroutine").into())
    }

    fn task_starters(&self) -> Vec<TaskStarter> {
        self.newproc1
            .map(|entry| TaskStarter {
                entry,
                names_at_entry: false,
            })
            .into_iter()
            .collect()
    }

    /// `newproc1` returns the new goroutine in rax, runnable or parked,
    /// before anything can run it.
    fn started_task(
        &self,
        stop: &dyn RuntimeStop,
        _starter: ImageAddress,
        registers: &RegisterFile,
    ) -> Result<Option<StartedTask>, Arc<str>> {
        let g = registers
            .get(RAX)
            .ok_or("the goroutine newproc1 returns is unavailable")?;
        if !self.allgs(stop)?.contains(&g) {
            return Err(format!("newproc1 returns {g:#x}, which is no goroutine").into());
        }

        let names = self.names(stop);
        self.goroutine(stop, names, g)?
            .map(|task| {
                Some(StartedTask {
                    task,
                    coroutine: None,
                })
            })
            .ok_or_else(|| format!("newproc1 returns {g:#x}, which is dead").into())
    }

    fn call_out(
        &self,
        entry: ImageAddress,
        registers: &RegisterFile,
    ) -> Option<Result<VirtualAddress, Arc<str>>> {
        let (_, register, name) = self
            .call_outs
            .iter()
            .find(|(address, ..)| *address == entry)?;
        Some(
            registers
                .get(*register)
                .map(VirtualAddress::new)
                .ok_or_else(|| format!("the function cgo calls, in {name}, is unknown").into()),
        )
    }

    fn task_stack(
        &self,
        stop: &dyn RuntimeStop,
        task: TaskRef,
    ) -> Result<Option<std::ops::Range<u64>>, Arc<str>> {
        self.find(stop, task)?
            .map(|(g, _)| self.stack(stop, g))
            .transpose()
    }

    fn dynamic_value(
        &self,
        stop: &dyn RuntimeStop,
        representation: &str,
        value: StoredValue<'_>,
    ) -> Option<Result<DynamicValue, Arc<str>>> {
        match &self.interfaces {
            Ok(interfaces) => interfaces.value(stop, representation, value),
            Err(missing) => Some(Err(Arc::clone(missing))),
        }
    }

    /// A fault the runtime cannot turn into a panic is fatal.
    fn signals(&self) -> RuntimeSignals {
        SIGNALS
    }

    fn hooks(&self) -> &[RuntimeHook] {
        self.hooks.all()
    }

    fn exception(
        &self,
        stop: &dyn RuntimeStop,
        hook: ImageAddress,
        registers: &RegisterFile,
    ) -> Result<RuntimeException, Arc<str>> {
        exceptions::exception(&self.hooks, &self.exceptions, stop, hook, registers)
    }

    /// The runtime's own traceback crosses the same switches
    /// (`runtime/traceback.go`): on a thread's system stack, with a
    /// goroutine on the thread, the frames go on at the registers the
    /// goroutine saved in `g.sched` when it switched.
    fn cross(
        &self,
        stop: &dyn RuntimeStop,
        thread: ThreadId,
        frame: &RegisterFile,
        after_call: bool,
    ) -> Result<Crossing, Arc<str>> {
        let pc = frame.get(RIP).ok_or("the frame's instruction is unknown")?;
        let name = self
            .image
            .function_name(stop.load_bias().wrapping_unload(VirtualAddress::new(pc)));
        let register = |number: DwarfRegister, name: &str| {
            frame
                .get(number)
                .ok_or_else(|| Arc::<str>::from(format!("the frame's {name} is unknown")))
        };

        let switch = match name.as_deref() {
            Some("runtime.systemstack") => Switch::Returns,
            Some("runtime.asmcgocall") => {
                return self.cross_cgo_call(stop, thread, frame, after_call);
            }
            Some("runtime.cgocallback") => return self.cross_callback(stop, thread, frame),
            Some("runtime.morestack" | "runtime.mcall") => Switch::Abandons,
            // A vDSO call keeps the goroutine's stack pointer in r12, which
            // C preserves, while it runs on the system stack. It saves it
            // before it publishes `m.vdsoSP` for the runtime's traceback,
            // and clears that after it is back, so until then the frame is
            // on the stack it was called on and r12 is its caller's.
            Some("runtime.nanotime1" | "runtime.vgetrandom1") => {
                let r12 = register(R12, "r12")?;
                let gs = self
                    .thread_gs(stop, thread)?
                    .ok_or("the thread runs no goroutine")?;
                let vdso_sp = word(
                    stop,
                    VirtualAddress::new(gs.m.wrapping_add(self.threads()?.m_vdso_sp)),
                )
                .ok_or("the thread's m is unreadable")?;

                // A signal's handler may make the call while the call it
                // interrupted has published its own.
                if vdso_sp == 0 || !self.stack(stop, gs.g)?.contains(&r12) {
                    return Ok(Crossing::Stay);
                }

                let mut registers = frame.clone();
                registers.set(RSP, r12);
                return Ok(Crossing::Resume(registers));
            }

            // A new thread begins on the stack `clone` gives it, whose
            // address is still in rsi; the thread that made it goes on on
            // its own.
            Some("runtime.clone") => {
                return Ok(if register(RSP, "rsp")? == register(RSI, "rsi")? {
                    Crossing::Outermost
                } else {
                    Crossing::Stay
                });
            }
            Some("gogo") => Switch::Resumes,
            name => {
                return Err(format!(
                    "{} switches stacks in a way the debugger does not follow",
                    name.unwrap_or("the function")
                )
                .into());
            }
        };

        let layout = self.goroutines()?;
        let gs = self
            .thread_gs(stop, thread)?
            .ok_or("the thread runs no goroutine")?;

        // Before a switch to the system stack, the frame is a call like any
        // other on the goroutine's stack, as one switching to a goroutine
        // is on the system stack until it has switched. The frame's own
        // stack says which, as the thread may since have gone on to its
        // signal stack.
        let on_system = self.stack(stop, gs.g0)?.contains(&register(RSP, "rsp")?);

        match switch {
            Switch::Resumes if on_system => return Ok(Crossing::Stay),
            Switch::Resumes => return Err("the thread is switching to a goroutine".into()),
            Switch::Returns | Switch::Abandons if !on_system => return Ok(Crossing::Stay),
            Switch::Returns | Switch::Abandons => {}
        }

        // A system stack with no goroutine begins at the switch.
        if gs.curg == 0 {
            return Ok(Crossing::Outermost);
        }

        self.listed(stop, gs.curg)?;
        let read = |offset: u64| {
            word(stop, VirtualAddress::new(gs.curg.wrapping_add(offset))).ok_or_else(|| {
                Arc::<str>::from(format!("the goroutine at {:#x} is unreadable", gs.curg))
            })
        };

        if read(layout.m)? != gs.m {
            return Err("the thread's goroutine is running on another thread".into());
        }

        let (saved_pc, sp, bp) = (
            read(layout.sched_pc)?,
            read(layout.sched_sp)?,
            read(layout.sched_bp)?,
        );
        if sp == 0 {
            return Err("the thread's goroutine saved no registers when it switched".into());
        }

        let mut registers = RegisterFile::new([(RSP, sp)]);
        if bp != 0 {
            registers.set(RBP, bp);
        }

        Ok(match switch {
            Switch::Returns => {
                registers.set(RIP, pc);
                Crossing::Resume(registers)
            }
            Switch::Abandons | Switch::Resumes if saved_pc == 0 => {
                return Err("the thread's goroutine saved no instruction".into());
            }
            Switch::Abandons | Switch::Resumes => {
                registers.set(RIP, saved_pc);
                Crossing::Continue(registers)
            }
        })
    }
}

impl GoRuntime {
    /// `asmcgocall` runs C on the system stack and returns to the
    /// goroutine's stack, as `systemstack` does. But Go that the C calls
    /// back runs on the goroutine meanwhile and saves its registers anew,
    /// so the goroutine's place is in `asmcgocall`'s frame on the system
    /// stack instead: the goroutine at 8(SP), and at 0(SP) how far below
    /// its stack's top its stack pointer was, which a callback's move of
    /// the stack keeps. Called on the system stack, it stores no goroutine
    /// and its caller's stack pointer instead. It stores them once it has
    /// switched, so only a frame in its call to C has them.
    fn cross_cgo_call(
        &self,
        stop: &dyn RuntimeStop,
        thread: ThreadId,
        frame: &RegisterFile,
        after_call: bool,
    ) -> Result<Crossing, Arc<str>> {
        let sp = frame.get(RSP).ok_or("the frame's rsp is unknown")?;
        let gs = self
            .thread_gs(stop, thread)?
            .ok_or("the thread runs no goroutine")?;
        if !self.stack(stop, gs.g0)?.contains(&sp) {
            return Ok(Crossing::Stay);
        }

        if !after_call {
            return Err("asmcgocall is between a goroutine's stack and the system stack".into());
        }

        let read = |address: u64, what: &str| {
            word(stop, VirtualAddress::new(address))
                .ok_or_else(|| Arc::<str>::from(format!("asmcgocall's {what} is unreadable")))
        };

        let g = read(sp.wrapping_add(8), "goroutine")?;
        let saved = read(sp, "saved stack pointer")?;
        let caller = if g == 0 {
            saved
        } else {
            self.listed(stop, g)?;
            let stack = self.stack(stop, g)?;
            stack
                .end
                .checked_sub(saved)
                .filter(|caller| stack.contains(caller))
                .ok_or_else(|| {
                    format!("asmcgocall's goroutine {g:#x} was {saved:#x} deep in no stack it has")
                })?
        };

        let mut registers = frame.clone();
        registers.set(RSP, caller);
        Ok(Crossing::Resume(registers))
    }

    /// `cgocallback` runs Go that C calls on the goroutine's stack, in a
    /// frame like the one it made on the system stack, whose stack pointer
    /// it saved in `m.g0.sched.sp`. Unwinding goes on there, through the C
    /// that called, rather than skip to the goroutine's frames before the
    /// C, as the runtime's own traceback does.
    fn cross_callback(
        &self,
        stop: &dyn RuntimeStop,
        thread: ThreadId,
        frame: &RegisterFile,
    ) -> Result<Crossing, Arc<str>> {
        let sp = frame.get(RSP).ok_or("the frame's rsp is unknown")?;
        let gs = self
            .thread_gs(stop, thread)?
            .ok_or("the thread runs no goroutine")?;
        let system = self.stack(stop, gs.g0)?;
        if system.contains(&sp) {
            return Ok(Crossing::Stay);
        }

        let layout = self.goroutines()?;
        let saved = word(
            stop,
            VirtualAddress::new(gs.g0.wrapping_add(layout.sched_sp)),
        )
        .ok_or("the system stack's saved stack pointer is unreadable")?;
        if !system.contains(&saved) {
            return Err(
                format!("cgocallback's frame at {saved:#x} is not on the system stack").into(),
            );
        }

        let mut registers = frame.clone();
        registers.set(RSP, saved);
        Ok(Crossing::Resume(registers))
    }
}

/// How a function that switches between a goroutine's stack and its
/// thread's system stack continues.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Switch {
    /// It switches to the system stack, and returns to its caller on the
    /// goroutine's stack once the function it runs there returns.
    Returns,

    /// It switches to the system stack and never returns; the goroutine
    /// resumes where it saved its registers.
    Abandons,

    /// It switches from the system stack to a goroutine's.
    Resumes,
}

/// The goroutines a thread's `m` knows, and the one in its thread-local
/// storage.
#[derive(Debug, Clone, Copy)]
struct ThreadGs {
    g: u64,
    m: u64,
    g0: u64,
    gsignal: u64,
    curg: u64,
}

impl GoRuntime {
    /// The g of a goroutine and the goroutine it is: at its locator, when
    /// the g there still has its number, or else the first g in `allgs`
    /// that does.
    fn find(
        &self,
        stop: &dyn RuntimeStop,
        task: TaskRef,
    ) -> Result<Option<(u64, RuntimeTask)>, Arc<str>> {
        let layout = self.goroutines()?;
        let names = self.names(stop);
        let number = task.number;
        let numbered = |g: u64| word(stop, VirtualAddress::new(g.wrapping_add(layout.goid)));
        if let Some(g) = task.locator
            && numbered(g) == Some(number)
            && let Ok(Some(found)) = self.goroutine(stop, names, g)
        {
            return Ok(Some((g, found)));
        }

        // An unreadable goroutine may be the one sought, so it is absent
        // only once every goroutine was read.
        let mut unreadable = None;

        for g in self.allgs(stop)? {
            match numbered(g) {
                Some(found) if found == number => match self.goroutine(stop, names, g) {
                    Ok(Some(task)) => return Ok(Some((g, task))),
                    Ok(None) => {}
                    Err(reason) => unreadable = unreadable.or(Some(reason)),
                },
                Some(_) => {}
                None => {
                    unreadable = unreadable
                        .or_else(|| Some(format!("the g at {g:#x} is unreadable").into()));
                }
            }
        }

        unreadable.map_or(Ok(None), |reason| {
            Err(format!("goroutine {number} was not found, and {reason}").into())
        })
    }

    fn activity(
        &self,
        stop: &dyn RuntimeStop,
        thread: ThreadId,
    ) -> Result<ThreadActivity, Arc<str>> {
        let layout = self.goroutines()?;
        let Some(gs) = self.thread_gs(stop, thread)? else {
            return Ok(ThreadActivity::Idle);
        };

        let (task, stack) = if gs.g == gs.g0 {
            (gs.curg, StackSegment::System)
        } else if gs.g == gs.gsignal {
            (gs.curg, StackSegment::Signal)
        } else {
            (gs.g, StackSegment::Task)
        };

        if task == 0 {
            return Ok(ThreadActivity::Idle);
        }

        self.listed(stop, task)?;
        let number = word(stop, VirtualAddress::new(task.wrapping_add(layout.goid)))
            .ok_or_else(|| format!("the thread's goroutine at {task:#x} is unreadable"))?;

        Ok(ThreadActivity::Task { number, stack })
    }

    /// The g in a thread's thread-local storage and the goroutines of its
    /// `m`, or `None` for a thread with no g.
    fn thread_gs(
        &self,
        stop: &dyn RuntimeStop,
        thread: ThreadId,
    ) -> Result<Option<ThreadGs>, Arc<str>> {
        let threads = self.threads()?;
        if let Some(instruction) = stop.instruction(thread) {
            let image = stop.load_bias().wrapping_unload(instruction);
            if self.starting.iter().any(|range| range.contains(image)) {
                return Err("the thread is starting, before its goroutine is set".into());
            }
        }

        let slot = stop.thread_local_address(thread, threads.tls_g)?;
        let g = word(stop, slot).ok_or("the thread's goroutine is unreadable")?;
        if g == 0 {
            return Ok(None);
        }

        let read = |address: u64| {
            word(stop, VirtualAddress::new(address)).ok_or_else(|| {
                Arc::<str>::from(format!("the thread's goroutine at {g:#x} is unreadable"))
            })
        };

        let m = read(g.wrapping_add(threads.g_m))?;
        if m == 0 {
            return Err(format!("the thread's goroutine at {g:#x} has no m").into());
        }

        Ok(Some(ThreadGs {
            g,
            m,
            g0: read(m.wrapping_add(threads.m_g0))?,
            gsignal: read(m.wrapping_add(threads.m_gsignal))?,
            curg: read(m.wrapping_add(threads.m_curg))?,
        }))
    }

    /// The bounds of the stack of the g at `g`.
    fn stack(&self, stop: &dyn RuntimeStop, g: u64) -> Result<std::ops::Range<u64>, Arc<str>> {
        let threads = self.threads()?;
        let read = |offset: u64| word(stop, VirtualAddress::new(g.wrapping_add(offset)));
        read(threads.g_stack_lo)
            .zip(read(threads.g_stack_hi))
            .map(|(lo, hi)| lo..hi)
            .ok_or_else(|| format!("the stack bounds of the g at {g:#x} are unreadable").into())
    }

    /// Fails unless `g` is a goroutine the runtime lists, so a corrupted
    /// pointer is never followed.
    fn listed(&self, stop: &dyn RuntimeStop, g: u64) -> Result<(), Arc<str>> {
        if self.allgs(stop)?.contains(&g) {
            Ok(())
        } else {
            Err(format!("the thread's goroutine at {g:#x} is not in runtime.allgs").into())
        }
    }
}

/// Which goroutines are the runtime's own work, as the runtime itself
/// decides: those that began in a runtime function, except the program's
/// main goroutine and a few that run the program's code.
#[derive(Debug, Default)]
struct Internal {
    /// Functions in the runtime whose goroutines run the program's code.
    program: Vec<ImageAddress>,
}

impl Internal {
    fn bind(image: &dyn RuntimeImage) -> Self {
        Self {
            program: [
                "runtime.main",
                "runtime.corostart",
                "runtime.handleAsyncEvent",
            ]
            .into_iter()
            .filter_map(|name| image.symbol(name).map(|symbol| symbol.address))
            .collect(),
        }
    }

    fn is_internal(&self, image: &dyn RuntimeImage, entry: ImageAddress) -> bool {
        !self.program.contains(&entry)
            && image
                .function_name(entry)
                .is_some_and(|name| name.starts_with("runtime."))
    }
}

/// The runtime's names for statuses and wait reasons, by number.
#[derive(Debug, Default)]
struct Names {
    statuses: Vec<Option<Arc<str>>>,
    wait_reasons: Vec<Option<Arc<str>>>,
}

impl Names {
    fn read(runtime: &GoRuntime, stop: &dyn RuntimeStop) -> Self {
        let strings = |name: &str| {
            let Some(symbol) = runtime.image.symbol(name) else {
                return Vec::new();
            };

            let Some(count) = symbol.size.map(|size| size / 16) else {
                return Vec::new();
            };

            let base = stop.load_bias().wrapping_load(symbol.address).get();
            (0..count.min(256))
                .map(|index| string(stop, VirtualAddress::new(base.wrapping_add(index * 16))))
                .collect()
        };

        Self {
            statuses: strings("runtime.gStatusStrings"),
            wait_reasons: strings("runtime.waitReasonStrings"),
        }
    }
}

fn read_unsigned(stop: &dyn RuntimeStop, address: VirtualAddress, size: usize) -> Option<u64> {
    let mut bytes = [0; 8];
    stop.read(address, bytes.get_mut(..size)?)
        .then(|| u64::from_le_bytes(bytes))
}

fn word(stop: &dyn RuntimeStop, address: VirtualAddress) -> Option<u64> {
    read_unsigned(stop, address, 8)
}

/// The profiler labels of the goroutine at `g`, in the order the program
/// gave them.
fn read_labels(stop: &dyn RuntimeStop, layout: &Labels, g: u64) -> Result<TaskLabels, Arc<str>> {
    let at = |address: u64| VirtualAddress::new(address);
    let set = word(stop, at(g.wrapping_add(layout.g_labels))).ok_or("g.labels is unreadable")?;
    if set == 0 {
        return Ok(Vec::new());
    }

    let layout = layout.set.as_ref().map_err(Arc::clone)?;
    let list = set.wrapping_add(layout.list);
    let (array, length) = word(stop, at(list))
        .zip(word(stop, at(list.wrapping_add(8))))
        .ok_or("the label set is unreadable")?;
    if length > MAX_LABELS {
        return Err(format!("{length} labels are more than {MAX_LABELS}").into());
    }

    (0..length)
        .map(|index| {
            let label = array.wrapping_add(index.wrapping_mul(layout.stride));
            let text = |offset: u64| {
                string(stop, at(label.wrapping_add(offset)))
                    .ok_or_else(|| Arc::<str>::from(format!("label {index} is unreadable")))
            };

            Ok((text(layout.key)?, text(layout.value)?))
        })
        .collect()
}

/// A Go string header's text, when it is short and readable.
fn string(stop: &dyn RuntimeStop, header: VirtualAddress) -> Option<Arc<str>> {
    let data = word(stop, header)?;
    let length = word(stop, VirtualAddress::new(header.get().wrapping_add(8)))?;
    let mut bytes = vec![
        0;
        usize::try_from(length)
            .ok()
            .filter(|length| *length <= 256)?
    ];
    stop.read(VirtualAddress::new(data), &mut bytes)
        .then_some(())?;
    String::from_utf8(bytes).ok().map(Arc::from)
}

#[cfg(test)]
mod tests;
