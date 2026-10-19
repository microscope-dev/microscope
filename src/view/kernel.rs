//! Kernels: WebAssembly functions that walk a structure which is an
//! algorithm more than a layout, such as a B-tree, for a view's generator
//! (`docs/views.md`).
//!
//! A kernel is a core WebAssembly module. It may import only
//! `microscope_kernel_v1`'s `read(address: i64, buffer: i32, length: i32) ->
//! i32` and `yield(words: i32, count: i32) -> i32`, and it exports
//! `run(arguments: i32, count: i32) -> i32` and its `memory`. A start
//! function, floats, SIMD, threads, or any other import keeps a module from
//! loading, so a kernel can do nothing but compute.
//!
//! Each call of `read` or `yield` stops the kernel, and whoever runs it
//! answers: the kernel never holds the program. A run is therefore a pure
//! function of its arguments and the bytes its reads returned, and a
//! [`Recording`] of them replays without a program.

use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex};

use wasmi::{
    CompilationMode, Config, EnforcedLimits, Engine, ExternType, Linker, Memory, Module, Store,
    StoreLimits, StoreLimitsBuilder, TypedFunc, TypedResumableCall, TypedResumableCallHostTrap,
    TypedResumableCallOutOfFuel, Val, ValType,
};

/// The module a kernel imports the host's functions from.
pub const ABI: &str = "microscope_kernel_v1";

/// The largest module a kernel may be.
pub const MAX_MODULE_BYTES: usize = 256 * 1024;

/// The most linear memory a kernel may use.
pub const MAX_MEMORY_BYTES: u64 = 4 * 1024 * 1024;

/// How deep a kernel's calls may nest.
pub const MAX_RECURSION: usize = 1024;

/// The most arguments a view passes a kernel.
pub const MAX_ARGUMENTS: usize = 32;

/// The most words one item may hold.
pub const MAX_WORDS: usize = 8;

/// The most bytes one `read` may ask for.
pub const MAX_READ: u32 = 64 * 1024;

/// How much fuel, about one WebAssembly instruction each, one unit of the
/// inspection's work buys.
pub const FUEL_PER_UNIT: u64 = 8;

/// How many units of work a run pays for at a time.
const UNITS_PER_REFILL: u64 = 512;

/// A WebAssembly page.
const PAGE: u64 = 64 * 1024;

/// A call the kernel made, which stops it until the host answers.
#[derive(Debug, Clone, Copy)]
enum Call {
    Read {
        address: u64,
        buffer: u32,
        length: u32,
    },
    Yield {
        words: u32,
        count: u32,
    },
}

impl fmt::Display for Call {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { .. } => formatter.write_str("read"),
            Self::Yield { .. } => formatter.write_str("yield"),
        }
    }
}

impl wasmi::errors::HostError for Call {}

/// A loaded kernel: its module, validated and compiled, and where its
/// source is.
pub struct Kernel {
    name: Arc<str>,
    source: Arc<str>,
    module: Module,
}

impl fmt::Debug for Kernel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Kernel")
            .field("name", &self.name)
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

impl Kernel {
    /// Loads the module `wasm` as the kernel `name`, whose source, or a
    /// link to it, is `source`. Says why a module cannot be a kernel.
    pub fn new(name: &str, source: &str, wasm: &[u8]) -> Result<Self, String> {
        if wasm.len() > MAX_MODULE_BYTES {
            return Err(format!(
                "the module is {} bytes, and a kernel may be at most {MAX_MODULE_BYTES}",
                wasm.len()
            ));
        }

        let mut config = Config::default();
        config
            .compilation_mode(CompilationMode::Eager)
            .consume_fuel(true)
            .floats(false)
            .allow_start_fn(false)
            .wasm_multi_memory(false)
            .set_max_recursion_depth(MAX_RECURSION)
            .enforced_limits(EnforcedLimits::strict());
        let engine = Engine::new(&config);
        let module = catch_unwind(AssertUnwindSafe(|| Module::new(&engine, wasm)))
            .map_err(|_| "the WebAssembly runtime failed loading it".to_owned())?
            .map_err(|error| format!("it is not a module a kernel may be: {error}"))?;
        check_interface(&module)?;
        Ok(Self {
            name: Arc::from(name),
            source: Arc::from(source),
            module,
        })
    }

    pub const fn name(&self) -> &Arc<str> {
        &self.name
    }

    /// The kernel's source, or a link to it.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Starts a run with `arguments`, recording it into `recordings` when
    /// given.
    pub fn start(&self, arguments: &[u64], recordings: Option<&Recordings>) -> Result<Run, String> {
        if arguments.len() > MAX_ARGUMENTS {
            return Err(format!(
                "it is given {} arguments, and a kernel takes at most {MAX_ARGUMENTS}",
                arguments.len()
            ));
        }

        let (store, memory, run, place) =
            catch_unwind(AssertUnwindSafe(|| self.instantiate(arguments)))
                .map_err(|_| "the WebAssembly runtime failed starting it".to_owned())??;
        let count = i32::try_from(arguments.len())
            .map_err(|_| "it is given too many arguments".to_owned())?;

        Ok(Run {
            store,
            memory,
            state: State::Ready {
                run,
                arguments: (place, count),
            },
            ended: None,
            recording: recordings.map(|recordings| {
                (
                    Recording {
                        kernel: self.name.to_string(),
                        arguments: arguments.to_vec(),
                        events: Vec::new(),
                        end: End::Stopped,
                    },
                    recordings.clone(),
                )
            }),
        })
    }

    /// A store holding an instance, with the arguments in a page of memory
    /// grown for them, whose address it also returns.
    #[expect(clippy::type_complexity, reason = "the pieces of one instance")]
    fn instantiate(
        &self,
        arguments: &[u64],
    ) -> Result<(Store<StoreLimits>, Memory, TypedFunc<(i32, i32), i32>, i32), String> {
        let engine = self.module.engine();
        let limits = StoreLimitsBuilder::new()
            .memory_size(
                usize::try_from(MAX_MEMORY_BYTES)
                    .map_err(|_| "its memory limit does not fit this host".to_owned())?,
            )
            .memories(1)
            .instances(1)
            .build();
        let mut store = Store::new(engine, limits);
        store.limiter(|limits| limits);
        store
            .set_fuel(0)
            .map_err(|error| format!("its fuel could not be set: {error}"))?;
        let linker = host_functions(engine)?;
        let instance = linker
            .instantiate_and_start(&mut store, &self.module)
            .map_err(|error| format!("it could not be instantiated: {error}"))?;
        let memory = instance
            .get_memory(&store, "memory")
            .ok_or("it exports no memory")?;
        let run = instance
            .get_typed_func::<(i32, i32), i32>(&store, "run")
            .map_err(|error| format!("its `run` cannot be called: {error}"))?;
        let pages = memory
            .grow(&mut store, 1)
            .map_err(|_| "its memory has no room for its arguments".to_owned())?;
        let place = pages
            .checked_mul(PAGE)
            .and_then(|place| i32::try_from(place).ok())
            .ok_or("its memory has no room for its arguments")?;
        let bytes = arguments
            .iter()
            .flat_map(|argument| argument.to_le_bytes())
            .collect::<Vec<_>>();

        memory
            .write(
                &mut store,
                usize::try_from(place)
                    .map_err(|_| "its arguments have no place in its memory".to_owned())?,
                &bytes,
            )
            .map_err(|error| format!("its arguments could not be written: {error}"))?;

        Ok((store, memory, run, place))
    }
}

/// The host functions, each of which stops the kernel with its call.
fn host_functions(engine: &Engine) -> Result<Linker<StoreLimits>, String> {
    let mut linker = Linker::new(engine);
    linker
        .func_wrap(
            ABI,
            "read",
            |address: i64, buffer: i32, length: i32| -> Result<i32, wasmi::Error> {
                Err(wasmi::Error::host(Call::Read {
                    address: address.cast_unsigned(),
                    buffer: buffer.cast_unsigned(),
                    length: length.cast_unsigned(),
                }))
            },
        )
        .map_err(|error| format!("its host functions could not be linked: {error}"))?;
    linker
        .func_wrap(
            ABI,
            "yield",
            |words: i32, count: i32| -> Result<i32, wasmi::Error> {
                Err(wasmi::Error::host(Call::Yield {
                    words: words.cast_unsigned(),
                    count: count.cast_unsigned(),
                }))
            },
        )
        .map_err(|error| format!("its host functions could not be linked: {error}"))?;
    Ok(linker)
}

/// Checks that a module imports only the host's functions, with their
/// types, and exports `run` and a memory no larger than a kernel's.
fn check_interface(module: &Module) -> Result<(), String> {
    use ValType::{I32, I64};
    for import in module.imports() {
        let (params, results): (&[ValType], &[ValType]) = match (import.module(), import.name()) {
            (ABI, "read") => (&[I64, I32, I32], &[I32]),
            (ABI, "yield") => (&[I32, I32], &[I32]),
            (module, name) => {
                return Err(format!(
                    "it imports `{module}.{name}`, and a kernel may import only `{ABI}.read` and `{ABI}.yield`"
                ));
            }
        };

        match import.ty() {
            ExternType::Func(ty) if ty.params() == params && ty.results() == results => {}
            _ => {
                return Err(format!(
                    "its `{ABI}.{}` does not have the host's type",
                    import.name()
                ));
            }
        }
    }

    let mut run = false;
    let mut memory = false;

    for export in module.exports() {
        match (export.name(), export.ty()) {
            ("run", ExternType::Func(ty)) => {
                if ty.params() != [I32, I32] || ty.results() != [I32] {
                    return Err("its `run` does not take two i32s and return one".to_owned());
                }

                run = true;
            }
            ("memory", ExternType::Memory(ty)) => {
                if ty.minimum().saturating_mul(PAGE) > MAX_MEMORY_BYTES {
                    return Err(format!(
                        "its memory starts larger than a kernel's may be, {MAX_MEMORY_BYTES} bytes"
                    ));
                }

                memory = true;
            }
            ("run" | "memory", _) => {
                return Err(format!(
                    "its `{}` is not what a kernel exports",
                    export.name()
                ));
            }
            _ => {}
        }
    }

    if !run {
        return Err("it exports no `run`".to_owned());
    }

    if !memory {
        return Err("it exports no `memory`".to_owned());
    }

    Ok(())
}

/// What answers a running kernel.
pub trait Host {
    type Error;

    /// Reads `length` bytes of the program's memory at `address`.
    fn read(&mut self, address: u64, length: usize) -> Result<Vec<u8>, Self::Error>;

    /// Pays for `units` of the kernel's work.
    fn pay(&mut self, units: u64) -> Result<(), Self::Error>;
}

/// Why a run ended before its kernel did.
#[derive(Debug)]
pub enum RunError<E> {
    /// The host could not answer: a read failed, or the budget ran out.
    Host(E),

    /// The kernel failed, and why.
    Kernel(String),
}

enum State {
    Ready {
        run: TypedFunc<(i32, i32), i32>,
        arguments: (i32, i32),
    },

    /// Stopped at a call, to be resumed with its answer.
    Called {
        call: TypedResumableCallHostTrap<i32>,
        answer: i32,
    },
    Hungry(TypedResumableCallOutOfFuel<i32>),
    Ended,
}

/// One run of a kernel, which yields its items one at a time.
pub struct Run {
    store: Store<StoreLimits>,
    memory: Memory,
    state: State,
    recording: Option<(Recording, Recordings)>,

    /// How the run ended, once it has.
    ended: Option<End>,
}

impl Run {
    /// Runs the kernel until it yields its next item, or `None` once it
    /// returns. A run that failed has ended.
    pub fn next<H: Host>(
        &mut self,
        host: &mut H,
    ) -> Result<Option<Box<[u64]>>, RunError<H::Error>> {
        if let Some(end) = &self.ended {
            return match end {
                End::Finished => Ok(None),
                End::Failed(reason) => Err(RunError::Kernel(reason.clone())),
                End::Stopped => Err(RunError::Kernel("its host stopped it".to_owned())),
            };
        }

        let result = self.advance(host);
        let end = match &result {
            Ok(Some(_)) => return result,
            Ok(None) => End::Finished,
            Err(RunError::Kernel(reason)) => End::Failed(reason.clone()),
            Err(RunError::Host(_)) => End::Stopped,
        };

        self.state = State::Ended;
        self.ended = Some(end.clone());
        if let Some((recording, _)) = &mut self.recording {
            recording.end = end;
        }

        result
    }

    fn advance<H: Host>(&mut self, host: &mut H) -> Result<Option<Box<[u64]>>, RunError<H::Error>> {
        loop {
            let state = std::mem::replace(&mut self.state, State::Ended);
            let store = &mut self.store;
            let step = catch_unwind(AssertUnwindSafe(|| match state {
                State::Ready { run, arguments } => Some(run.call_resumable(store, arguments)),
                State::Called { call, answer } => Some(call.resume(store, &[Val::I32(answer)])),
                State::Hungry(call) => Some(call.resume(store)),
                State::Ended => None,
            }))
            .map_err(|_| {
                RunError::Kernel("the WebAssembly runtime failed running it".to_owned())
            })?;
            let Some(step) = step else {
                return Ok(None);
            };

            // A reason is one line, as a recording writes it.
            let trapped = |error: wasmi::Error| {
                RunError::Kernel(format!("it trapped: {error}").replace('\n', " "))
            };

            match step.map_err(trapped)? {
                TypedResumableCall::Finished(0) => return Ok(None),
                TypedResumableCall::Finished(status) => {
                    return Err(RunError::Kernel(format!("it returned {status}")));
                }
                TypedResumableCall::OutOfFuel(call) => {
                    let units = call
                        .required_fuel()
                        .div_ceil(FUEL_PER_UNIT)
                        .max(UNITS_PER_REFILL);

                    host.pay(units).map_err(RunError::Host)?;
                    let left = self.store.get_fuel().unwrap_or(0);
                    self.store
                        .set_fuel(left.saturating_add(units.saturating_mul(FUEL_PER_UNIT)))
                        .map_err(|error| {
                            RunError::Kernel(format!("its fuel could not be set: {error}"))
                        })?;
                    self.state = State::Hungry(call);
                }
                TypedResumableCall::HostTrap(call) => {
                    let Some(&request) = call.host_error().downcast_ref::<Call>() else {
                        return Err(RunError::Kernel("it stopped for no call".to_owned()));
                    };

                    match request {
                        Call::Read {
                            address,
                            buffer,
                            length,
                        } => {
                            let answer = self.read(host, address, buffer, length)?;
                            self.state = State::Called { call, answer };
                        }
                        Call::Yield { words, count } => {
                            let item = self.item(words, count)?;
                            if let Some((recording, _)) = &mut self.recording {
                                recording.events.push(Event::Item(item.to_vec()));
                            }

                            self.state = State::Called { call, answer: 1 };
                            return Ok(Some(item));
                        }
                    }
                }
            }
        }
    }

    /// Answers a read: the bytes, in the kernel's buffer, and their count.
    fn read<H: Host>(
        &mut self,
        host: &mut H,
        address: u64,
        buffer: u32,
        length: u32,
    ) -> Result<i32, RunError<H::Error>> {
        if length > MAX_READ {
            return Err(RunError::Kernel(format!(
                "it read {length} bytes at once, and a kernel reads at most {MAX_READ}"
            )));
        }

        let start = buffer as usize;
        let size = length as usize;
        if start
            .checked_add(size)
            .is_none_or(|end| end > self.memory.data(&self.store).len())
        {
            return Err(RunError::Kernel(format!(
                "it read into {length} bytes at {buffer:#x}, past its memory"
            )));
        }

        let bytes = host.read(address, size).map_err(RunError::Host)?;
        if bytes.len() != size {
            return Err(RunError::Kernel(format!(
                "a read of {size} bytes returned {}",
                bytes.len()
            )));
        }

        self.memory.data_mut(&mut self.store)[start..start + size].copy_from_slice(&bytes);
        if let Some((recording, _)) = &mut self.recording {
            recording.events.push(Event::Read { address, bytes });
        }

        Ok(length.cast_signed())
    }

    /// The words of a yielded item.
    fn item<E>(&self, words: u32, count: u32) -> Result<Box<[u64]>, RunError<E>> {
        let count = count as usize;
        if count == 0 || count > MAX_WORDS {
            return Err(RunError::Kernel(format!(
                "it yielded {count} words, and an item holds 1 to {MAX_WORDS}"
            )));
        }

        let start = words as usize;
        let bytes = self
            .memory
            .data(&self.store)
            .get(start..start + count * 8)
            .ok_or_else(|| {
                RunError::Kernel(format!("it yielded words at {words:#x}, past its memory"))
            })?;

        Ok(bytes
            .as_chunks::<8>()
            .0
            .iter()
            .map(|word| u64::from_le_bytes(*word))
            .collect())
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        if let Some((recording, recordings)) = self.recording.take() {
            recordings.keep(recording);
        }
    }
}

/// The recordings of the runs of one inspection, in the order they ended.
#[derive(Debug, Clone, Default)]
pub struct Recordings(Arc<Mutex<Vec<Recording>>>);

impl PartialEq for Recordings {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for Recordings {}

impl Recordings {
    fn keep(&self, recording: Recording) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(recording);
    }

    /// The recordings kept so far, which it forgets.
    pub fn take(&self) -> Vec<Recording> {
        std::mem::take(
            &mut *self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
}

/// What a run read or yielded, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Read { address: u64, bytes: Vec<u8> },
    Item(Vec<u64>),
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum End {
    /// The kernel returned 0.
    Finished,

    /// The kernel failed, and why.
    Failed(String),

    /// Its host stopped running it: it had the items it wanted, a read
    /// failed, or the budget ran out.
    Stopped,
}

/// A run, as it happened: everything it depends on and everything it
/// gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recording {
    pub kernel: String,
    pub arguments: Vec<u64>,
    pub events: Vec<Event>,
    pub end: End,
}

/// The first line of a recording's text.
const HEADER: &str = "microscope-kernel-run 1";

impl fmt::Display for Recording {
    /// A recording as text: a header, then a line for the kernel, its
    /// arguments, each event, and the end, numbers in hexadecimal.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(formatter, "{HEADER}")?;
        writeln!(formatter, "kernel {}", self.kernel)?;
        write!(formatter, "arguments")?;
        for argument in &self.arguments {
            write!(formatter, " {argument:#x}")?;
        }

        writeln!(formatter)?;
        for event in &self.events {
            match event {
                Event::Read { address, bytes } => {
                    write!(formatter, "read {address:#x} ")?;
                    for byte in bytes {
                        write!(formatter, "{byte:02x}")?;
                    }

                    writeln!(formatter)?;
                }
                Event::Item(words) => {
                    write!(formatter, "item")?;
                    for word in words {
                        write!(formatter, " {word:#x}")?;
                    }

                    writeln!(formatter)?;
                }
            }
        }

        match &self.end {
            End::Finished => writeln!(formatter, "end finished"),
            End::Failed(reason) => writeln!(formatter, "end failed {reason}"),
            End::Stopped => writeln!(formatter, "end stopped"),
        }
    }
}

/// Reads the recordings in `text`, each written as [`Recording`]'s
/// `Display` writes one.
pub fn parse_recordings(text: &str) -> Result<Vec<Recording>, String> {
    let mut recordings = Vec::new();
    let mut lines = text
        .lines()
        .enumerate()
        .map(|(index, line)| (index + 1, line));

    while let Some((number, line)) = lines.next() {
        if line.trim().is_empty() {
            continue;
        }

        if line != HEADER {
            return Err(format!("line {number}: expected `{HEADER}`"));
        }

        let mut field = |name: &str| match lines.next() {
            Some((number, line)) => line
                .strip_prefix(name)
                .filter(|rest| rest.is_empty() || rest.starts_with(' '))
                .map(|rest| (number, rest.trim().to_owned()))
                .ok_or_else(|| format!("line {number}: expected `{name}`")),
            None => Err(format!("the recording ends before its `{name}`")),
        };

        let (_, kernel) = field("kernel")?;
        let (number, arguments) = field("arguments")?;
        let arguments = arguments
            .split_whitespace()
            .map(|word| {
                hexadecimal(word).ok_or_else(|| format!("line {number}: `{word}` is no number"))
            })
            .collect::<Result<Vec<_>, _>>()?;

        // A run is only what a host could have given and taken.
        if arguments.len() > MAX_ARGUMENTS {
            return Err(format!(
                "line {number}: a kernel takes at most {MAX_ARGUMENTS} arguments"
            ));
        }

        let mut events = Vec::new();
        let end = loop {
            let Some((number, line)) = lines.next() else {
                return Err("the recording has no end".to_owned());
            };

            let bad = || format!("line {number}: `{line}` is no event");
            let (word, rest) = line.split_once(' ').unwrap_or((line, ""));

            match word {
                "read" => {
                    let (address, bytes) = rest.split_once(' ').unwrap_or((rest, ""));
                    let address = hexadecimal(address).ok_or_else(bad)?;
                    let most = MAX_READ as usize * 2;
                    if bytes.len() % 2 != 0 || bytes.len() > most {
                        return Err(bad());
                    }

                    let bytes = (0..bytes.len())
                        .step_by(2)
                        .map(|at| u8::from_str_radix(bytes.get(at..at + 2)?, 16).ok())
                        .collect::<Option<Vec<_>>>()
                        .ok_or_else(bad)?;

                    events.push(Event::Read { address, bytes });
                }
                "item" => {
                    let words = rest
                        .split_whitespace()
                        .map(hexadecimal)
                        .collect::<Option<Vec<_>>>()
                        .filter(|words| (1..=MAX_WORDS).contains(&words.len()))
                        .ok_or_else(bad)?;

                    events.push(Event::Item(words));
                }
                "end" => {
                    break match rest.split_once(' ').unwrap_or((rest, "")) {
                        ("finished", "") => End::Finished,
                        ("stopped", "") => End::Stopped,
                        ("failed", reason) => End::Failed(reason.to_owned()),
                        _ => return Err(bad()),
                    };
                }
                _ => return Err(bad()),
            }
        };

        recordings.push(Recording {
            kernel,
            arguments,
            events,
            end,
        });
    }

    Ok(recordings)
}

fn hexadecimal(word: &str) -> Option<u64> {
    u64::from_str_radix(word.strip_prefix("0x")?, 16).ok()
}

/// How much work a replay may do: as much as the largest inspection.
const REPLAY_UNITS: u64 = crate::inspection::MAX_INSPECTION_LIMITS.expression_work;

/// Why a replaying host stopped answering.
enum Halt {
    /// The recording stopped here.
    End,
    Differs(String),
}

/// A host that answers a kernel with what a recording says.
struct Replayer<'r> {
    recording: &'r Recording,
    next: usize,
    units: u64,
}

impl Replayer<'_> {
    /// Whether the recording's host stopped the run here.
    fn stops(&self) -> bool {
        self.next == self.recording.events.len() && self.recording.end == End::Stopped
    }

    /// What the recording does next.
    fn recorded(&self) -> String {
        match self.recording.events.get(self.next) {
            Some(Event::Read { address, bytes }) => {
                format!("reads {} bytes at {address:#x}", bytes.len())
            }
            Some(Event::Item(words)) => format!("yields {}", words_text(words)),
            None => match &self.recording.end {
                End::Finished => "finishes".to_owned(),
                End::Failed(reason) => format!("fails: {reason}"),
                End::Stopped => "stops".to_owned(),
            },
        }
    }
}

impl Host for Replayer<'_> {
    type Error = Halt;

    fn read(&mut self, address: u64, length: usize) -> Result<Vec<u8>, Halt> {
        match self.recording.events.get(self.next) {
            Some(Event::Read {
                address: recorded,
                bytes,
            }) if *recorded == address && bytes.len() == length => {
                self.next += 1;
                Ok(bytes.clone())
            }
            _ if self.stops() => Err(Halt::End),
            _ => Err(Halt::Differs(format!(
                "event {}: the kernel reads {length} bytes at {address:#x}, and the recording {}",
                self.next,
                self.recorded()
            ))),
        }
    }

    fn pay(&mut self, units: u64) -> Result<(), Halt> {
        self.units = self.units.saturating_add(units);
        if self.units > REPLAY_UNITS {
            return Err(Halt::Differs(
                "the kernel does more work than any inspection allows".to_owned(),
            ));
        }

        Ok(())
    }
}

fn words_text(words: &[u64]) -> String {
    words
        .iter()
        .map(|word| format!("{word:#x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Runs `kernel` again as `recording` ran, answering its reads with the
/// bytes recorded, and returns how many events it reproduced, or the first
/// difference. A recording its host stopped is reproduced once all its
/// events are.
pub fn replay(kernel: &Kernel, recording: &Recording) -> Result<usize, String> {
    let mut run = kernel.start(&recording.arguments, None)?;
    let mut host = Replayer {
        recording,
        next: 0,
        units: 0,
    };

    loop {
        let ended = match run.next(&mut host) {
            Ok(Some(item)) => match recording.events.get(host.next) {
                Some(Event::Item(words)) if **words == *item => {
                    host.next += 1;
                    continue;
                }
                _ if host.stops() => return Ok(host.next),
                _ => {
                    return Err(format!(
                        "event {}: the kernel yields {}, and the recording {}",
                        host.next,
                        words_text(&item),
                        host.recorded()
                    ));
                }
            },
            Err(RunError::Host(Halt::End)) => return Ok(host.next),
            Err(RunError::Host(Halt::Differs(difference))) => return Err(difference),
            Ok(None) => End::Finished,
            Err(RunError::Kernel(reason)) => End::Failed(reason),
        };

        let reproduced = host.next == recording.events.len()
            && (recording.end == End::Stopped || recording.end == ended);
        if reproduced {
            return Ok(host.next);
        }

        let ended = match ended {
            End::Failed(reason) => format!("fails: {reason}"),
            _ => "finishes".to_owned(),
        };

        return Err(format!(
            "event {}: the kernel {ended}, and the recording {}",
            host.next,
            host.recorded()
        ));
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    /// A WebAssembly module, assembled by hand: a kernel's two imports,
    /// then `run`, whose body is given, and any other functions.
    struct Wasm {
        imports: Vec<(&'static str, &'static str, u8)>,

        /// Each function's type and body, after `run`'s.
        functions: Vec<(u8, Vec<u8>)>,
        memory: Option<u32>,
        exports: Vec<(&'static str, u8, u32)>,
        start: Option<u32>,
    }

    /// The types functions have: `read`'s, `yield`'s, `run`'s, and `() ->
    /// ()`.
    const READ: u8 = 0;
    const YIELD: u8 = 1;
    const RUN: u8 = 2;
    const NOTHING: u8 = 3;

    /// The index of `run`, after the two imports.
    const RUN_INDEX: u32 = 2;

    fn leb(mut value: u64, bytes: &mut Vec<u8>) {
        loop {
            let byte = u8::try_from(value & 0x7f).expect("seven bits");
            value >>= 7;
            if value == 0 {
                bytes.push(byte);
                return;
            }

            bytes.push(byte | 0x80);
        }
    }

    fn sleb(mut value: i64, bytes: &mut Vec<u8>) {
        loop {
            let byte = u8::try_from(value & 0x7f).expect("seven bits");
            value >>= 7;
            let done = (value == 0 && byte & 0x40 == 0) || (value == -1 && byte & 0x40 != 0);
            bytes.push(if done { byte } else { byte | 0x80 });
            if done {
                return;
            }
        }
    }

    fn name(text: &str, bytes: &mut Vec<u8>) {
        leb(text.len() as u64, bytes);
        bytes.extend(text.as_bytes());
    }

    fn section(id: u8, contents: &[u8], bytes: &mut Vec<u8>) {
        bytes.push(id);
        leb(contents.len() as u64, bytes);
        bytes.extend(contents);
    }

    impl Wasm {
        /// A kernel whose `run` has `body`, with one local i64.
        fn kernel(body: &[u8]) -> Self {
            Self {
                imports: vec![(ABI, "read", READ), (ABI, "yield", YIELD)],
                functions: vec![(RUN, body.to_vec())],
                memory: Some(1),
                exports: vec![("run", 0, RUN_INDEX), ("memory", 2, 0)],
                start: None,
            }
        }

        fn bytes(&self) -> Vec<u8> {
            const I32: u8 = 0x7f;
            const I64: u8 = 0x7e;
            let mut bytes = b"\0asm\x01\0\0\0".to_vec();
            let types: [(&[u8], &[u8]); 4] = [
                (&[I64, I32, I32], &[I32]),
                (&[I32, I32], &[I32]),
                (&[I32, I32], &[I32]),
                (&[], &[]),
            ];
            let mut contents = Vec::new();
            leb(types.len() as u64, &mut contents);
            for (params, results) in types {
                contents.push(0x60);
                leb(params.len() as u64, &mut contents);
                contents.extend(params);
                leb(results.len() as u64, &mut contents);
                contents.extend(results);
            }

            section(1, &contents, &mut bytes);
            let mut contents = Vec::new();
            leb(self.imports.len() as u64, &mut contents);
            for (module, field, ty) in &self.imports {
                name(module, &mut contents);
                name(field, &mut contents);
                contents.extend([0, *ty]);
            }

            section(2, &contents, &mut bytes);
            let mut contents = Vec::new();
            leb(self.functions.len() as u64, &mut contents);
            for (ty, _) in &self.functions {
                contents.push(*ty);
            }

            section(3, &contents, &mut bytes);
            if let Some(pages) = self.memory {
                let mut contents = vec![1, 0];
                leb(u64::from(pages), &mut contents);
                section(5, &contents, &mut bytes);
            }

            let mut contents = Vec::new();
            leb(self.exports.len() as u64, &mut contents);
            for (field, kind, index) in &self.exports {
                name(field, &mut contents);
                contents.push(*kind);
                leb(u64::from(*index), &mut contents);
            }

            section(7, &contents, &mut bytes);
            if let Some(start) = self.start {
                let mut contents = Vec::new();
                leb(u64::from(start), &mut contents);
                section(8, &contents, &mut bytes);
            }

            let mut contents = Vec::new();
            leb(self.functions.len() as u64, &mut contents);
            for (_, body) in &self.functions {
                // One local i64, then the body.
                let mut function = vec![1, 1, I64];
                function.extend(body);
                function.push(0x0b);
                leb(function.len() as u64, &mut contents);
                contents.extend(function);
            }

            section(10, &contents, &mut bytes);
            bytes
        }
    }

    fn i32_const(value: i32) -> Vec<u8> {
        let mut bytes = vec![0x41];
        sleb(i64::from(value), &mut bytes);
        bytes
    }

    fn i64_const(value: i64) -> Vec<u8> {
        let mut bytes = vec![0x42];
        sleb(value, &mut bytes);
        bytes
    }

    const ARGUMENTS: u8 = 0;
    const COUNTER: u8 = 2;

    /// Yields the items `[0]`, `[1]`, …, below its first argument, from
    /// address 0, stopping when the host wants no more.
    fn counter() -> Vec<u8> {
        let mut body = vec![0x02, 0x40, 0x03, 0x40]; // block, loop
        body.extend([0x20, COUNTER, 0x20, ARGUMENTS, 0x29, 3, 0, 0x5a, 0x0d, 1]); // i >= args[0]: break
        body.extend(i32_const(0));
        body.extend([0x20, COUNTER, 0x37, 3, 0]); // memory[0] = i
        body.extend(i32_const(0));
        body.extend(i32_const(1));
        body.extend([0x10, 1, 0x45, 0x0d, 1]); // yield(0, 1) == 0: break
        body.extend([0x20, COUNTER]);
        body.extend(i64_const(1));
        body.extend([0x7c, 0x21, COUNTER, 0x0c, 0, 0x0b, 0x0b]); // i += 1, again
        body.extend(i32_const(0));
        body
    }

    /// Reads the word at its first argument's address into address 8 and
    /// yields it.
    fn reader() -> Vec<u8> {
        let mut body = vec![0x20, ARGUMENTS, 0x29, 3, 0];
        body.extend(i32_const(8));
        body.extend(i32_const(8));
        body.extend([0x10, 0, 0x1a]);
        body.extend(i32_const(8));
        body.extend(i32_const(1));
        body.extend([0x10, 1, 0x1a]);
        body.extend(i32_const(0));
        body
    }

    /// Program memory: bytes by address, and the work paid for.
    #[derive(Default)]
    struct Memory {
        bytes: BTreeMap<u64, u8>,
        units: u64,
        budget: Option<u64>,
    }

    impl Memory {
        fn write(&mut self, address: u64, bytes: &[u8]) {
            for (offset, byte) in bytes.iter().enumerate() {
                self.bytes.insert(address + offset as u64, *byte);
            }
        }
    }

    impl Host for Memory {
        type Error = String;

        fn read(&mut self, address: u64, length: usize) -> Result<Vec<u8>, String> {
            (address..address + length as u64)
                .map(|at| {
                    self.bytes
                        .get(&at)
                        .copied()
                        .ok_or_else(|| format!("{at:#x} is unmapped"))
                })
                .collect()
        }

        fn pay(&mut self, units: u64) -> Result<(), String> {
            self.units += units;
            match self.budget {
                Some(budget) if self.units > budget => Err("out of budget".to_owned()),
                _ => Ok(()),
            }
        }
    }

    fn load(wasm: &Wasm) -> Result<Kernel, String> {
        Kernel::new("test", "tests", &wasm.bytes())
    }

    /// Every item of a run, and how it ended.
    fn items(
        kernel: &Kernel,
        arguments: &[u64],
        memory: &mut Memory,
    ) -> (Vec<Vec<u64>>, Result<(), String>) {
        let mut run = kernel.start(arguments, None).expect("starts");
        let mut items = Vec::new();

        loop {
            match run.next(memory) {
                Ok(Some(item)) => items.push(item.to_vec()),
                Ok(None) => return (items, Ok(())),
                Err(RunError::Host(error) | RunError::Kernel(error)) => return (items, Err(error)),
            }
        }
    }

    #[test]
    fn modules_that_could_do_more_than_compute_do_not_load() {
        let returns = i32_const(0);
        let cases: Vec<(Wasm, &str)> = vec![
            (
                Wasm {
                    imports: vec![
                        ("wasi_snapshot_preview1", "fd_write", YIELD),
                        (ABI, "yield", YIELD),
                    ],
                    ..Wasm::kernel(&returns)
                },
                "imports `wasi_snapshot_preview1.fd_write`",
            ),
            (
                Wasm {
                    imports: vec![(ABI, "read", YIELD), (ABI, "yield", YIELD)],
                    ..Wasm::kernel(&returns)
                },
                "`microscope_kernel_v1.read` does not have the host's type",
            ),
            (
                Wasm {
                    functions: vec![(RUN, returns.clone()), (NOTHING, Vec::new())],
                    start: Some(3),
                    ..Wasm::kernel(&returns)
                },
                "start",
            ),
            (
                Wasm::kernel(
                    &[[0x44].as_slice(), &1.0_f64.to_le_bytes(), &[0x1a], &returns].concat(),
                ),
                "float",
            ),
            (
                Wasm {
                    memory: Some(65),
                    ..Wasm::kernel(&returns)
                },
                "its memory starts larger than a kernel's may be",
            ),
            (
                Wasm {
                    exports: vec![("memory", 2, 0)],
                    ..Wasm::kernel(&returns)
                },
                "it exports no `run`",
            ),
            (
                Wasm {
                    memory: None,
                    exports: vec![("run", 0, RUN_INDEX)],
                    ..Wasm::kernel(&returns)
                },
                "it exports no `memory`",
            ),
            (
                Wasm {
                    exports: vec![("run", 0, 0), ("memory", 2, 0)],
                    ..Wasm::kernel(&returns)
                },
                "its `run` does not take two i32s and return one",
            ),
        ];

        for (wasm, expected) in cases {
            let error = load(&wasm).expect_err(expected);
            assert!(error.contains(expected), "{error}: not {expected}");
        }

        let large =
            Kernel::new("large", "tests", &vec![0; MAX_MODULE_BYTES + 1]).expect_err("large");
        assert!(large.contains("at most 262144"), "{large}");
    }

    #[test]
    fn a_run_reads_yields_and_pays_for_its_work_until_it_returns() {
        let mut memory = Memory::default();
        memory.write(0x1000, &0xfeed_u64.to_le_bytes());
        let kernel = load(&Wasm::kernel(&reader())).expect("loads");
        assert_eq!(
            items(&kernel, &[0x1000], &mut memory),
            (vec![vec![0xfeed]], Ok(()))
        );
        assert!(memory.units > 0, "it paid for its work");

        // A read the host cannot make ends the run with the host's error.
        let (_, ended) = items(&kernel, &[0x2000], &mut memory);
        assert_eq!(ended, Err("0x2000 is unmapped".to_owned()));

        let kernel = load(&Wasm::kernel(&counter())).expect("loads");
        assert_eq!(
            items(&kernel, &[3], &mut Memory::default()),
            (vec![vec![0], vec![1], vec![2]], Ok(()))
        );
    }

    #[test]
    fn kernels_that_run_away_or_misbehave_end_with_why() {
        // A loop ends when its budget does, at the same point every time.
        let spins = [[0x03, 0x40, 0x0c, 0, 0x0b].as_slice(), &i32_const(0)].concat();
        let kernel = load(&Wasm::kernel(&spins)).expect("loads");
        let mut memory = Memory {
            budget: Some(10_000),
            ..Memory::default()
        };

        assert_eq!(
            items(&kernel, &[], &mut memory).1,
            Err("out of budget".to_owned())
        );
        let spent = memory.units;
        let mut again = Memory {
            budget: Some(10_000),
            ..Memory::default()
        };

        let _ = items(&kernel, &[], &mut again);
        assert_eq!(again.units, spent);

        let recurses = [0x20, 0, 0x20, 1, 0x10, 2];
        let cases: [(Vec<u8>, &str); 5] = [
            (recurses.to_vec(), "it trapped"),
            ([[0x00].as_slice(), &i32_const(0)].concat(), "it trapped"),
            (i32_const(3), "it returned 3"),
            (
                [i32_const(0), i32_const(9), vec![0x10, 1]].concat(),
                "it yielded 9 words, and an item holds 1 to 8",
            ),
            (
                [
                    i64_const(0),
                    i32_const(131_000),
                    i32_const(100),
                    vec![0x10, 0],
                ]
                .concat(),
                "past its memory",
            ),
        ];

        for (body, expected) in cases {
            let kernel = load(&Wasm::kernel(&body)).expect("loads");
            let (_, ended) = items(&kernel, &[], &mut Memory::default());
            let error = ended.expect_err(expected);
            assert!(error.contains(expected), "{error}: not {expected}");
        }

        // A run that failed says so again, rather than seeming to end well.
        let kernel = load(&Wasm::kernel(&i32_const(3))).expect("loads");
        let mut run = kernel.start(&[], None).expect("starts");

        for _ in 0..2 {
            assert!(matches!(
                run.next(&mut Memory::default()),
                Err(RunError::Kernel(reason)) if reason == "it returned 3"
            ));
        }
    }

    #[test]
    fn a_recorded_run_replays_and_a_changed_recording_does_not() {
        let mut memory = Memory::default();
        memory.write(0x1000, &0xfeed_u64.to_le_bytes());
        let kernel = load(&Wasm::kernel(&reader())).expect("loads");
        let recordings = Recordings::default();
        let mut run = kernel.start(&[0x1000], Some(&recordings)).expect("starts");
        while run.next(&mut memory).expect("runs").is_some() {}
        drop(run);
        let [recording] = <[Recording; 1]>::try_from(recordings.take()).expect("one run");
        let text = recording.to_string();
        assert_eq!(
            text,
            "microscope-kernel-run 1\nkernel test\narguments 0x1000\nread 0x1000 edfe000000000000\nitem 0xfeed\nend finished\n"
        );
        assert_eq!(parse_recordings(&text), Ok(vec![recording.clone()]));
        assert_eq!(replay(&kernel, &recording), Ok(2));

        let changed =
            parse_recordings(&text.replace("item 0xfeed", "item 0xbeef")).expect("parses");
        assert_eq!(
            replay(&kernel, &changed[0]),
            Err("event 1: the kernel yields 0xfeed, and the recording yields 0xbeef".to_owned())
        );
        let moved = parse_recordings(&text.replace("read 0x1000", "read 0x1008")).expect("parses");
        assert_eq!(
            replay(&kernel, &moved[0]),
            Err("event 0: the kernel reads 8 bytes at 0x1000, and the recording reads 8 bytes at 0x1008".to_owned())
        );

        // A run its host stopped replays as far as it went.
        let stopped = parse_recordings(
            "microscope-kernel-run 1\nkernel test\narguments 0x1000\nread 0x1000 edfe000000000000\nend stopped\n",
        )
        .expect("parses");
        assert_eq!(replay(&kernel, &stopped[0]), Ok(1));

        // What no host could have given or taken is no recording.
        let run =
            |body: &str| format!("microscope-kernel-run 1\nkernel test\n{body}\nend stopped\n");

        for body in [
            "arguments x".to_owned(),
            format!("arguments{}", " 0x0".repeat(MAX_ARGUMENTS + 1)),
            "arguments\nitem".to_owned(),
            format!("arguments\nitem{}", " 0x0".repeat(MAX_WORDS + 1)),
            format!("arguments\nread 0x0 {}", "00".repeat(MAX_READ as usize + 1)),
        ] {
            assert!(parse_recordings(&run(&body)).is_err(), "{body}");
        }

        assert!(parse_recordings(&run("arguments\nitem 0x1")).is_ok());
        assert!(parse_recordings("microscope-kernel-run 1\nkernelfoo test\n").is_err());
    }

    /// A Rust B-tree laid out in `memory`: leaves of `fill` keys, each key
    /// the u64 that is its entry's position, under `height` levels of
    /// internal nodes with `fill` keys each. Returns its root, its entries'
    /// key addresses in order, and its layout's arguments without the
    /// root and height.
    fn btree(
        memory: &mut Memory,
        height: u64,
        fill: u16,
        next: &mut u64,
        position: &mut u64,
    ) -> (u64, Vec<u64>) {
        // A node: len (u16) at 0, keys (u64) at 8, values (u32) at 96,
        // edges at 144.
        let node = *next;
        *next += 0x200;
        memory.write(node, &fill.to_le_bytes());
        let mut keys = Vec::new();

        for index in 0..=u64::from(fill) {
            if height > 0 {
                let (child, below) = btree(memory, height - 1, fill, next, position);
                memory.write(node + 144 + index * 8, &child.to_le_bytes());
                keys.extend(below);
            }

            if index < u64::from(fill) {
                memory.write(node + 8 + index * 8, &position.to_le_bytes());
                keys.push(node + 8 + index * 8);
                *position += 1;
            }
        }

        (node, keys)
    }

    #[test]
    fn the_rust_btree_kernel_walks_every_entry_in_order() {
        let kernel = crate::view::ViewSet::built_in()
            .kernel("rust-btree")
            .expect("a built-in kernel");
        let layout = |root: u64, height: u64| vec![root, height, 0, 8, 96, 8, 4, 144, 11];

        for (height, fill) in [(0, 0), (0, 1), (0, 11), (1, 5), (2, 11), (3, 2)] {
            let mut memory = Memory::default();
            let (root, keys) = btree(&mut memory, height, fill, &mut 0x10000, &mut 0);
            let (items, ended) = items(&kernel, &layout(root, height), &mut memory);
            assert_eq!(ended, Ok(()), "height {height}");
            assert_eq!(
                items.iter().map(|item| item[0]).collect::<Vec<_>>(),
                keys,
                "height {height}, fill {fill}"
            );
            for (item, key) in items.iter().zip(&keys) {
                let node = key - (key - 0x10000) % 0x200;
                let index = (key - node - 8) / 8;
                assert_eq!(item[1], node + 96 + index * 4);
            }

            let values = items
                .iter()
                .map(|item| {
                    u64::from_le_bytes(
                        memory
                            .read(item[0], 8)
                            .expect("a key")
                            .try_into()
                            .expect("8"),
                    )
                })
                .collect::<Vec<_>>();
            assert_eq!(values, (0..keys.len() as u64).collect::<Vec<_>>());
        }

        let mut memory = Memory::default();
        let (root, _) = btree(&mut memory, 1, 2, &mut 0x10000, &mut 0);
        memory.write(root, &12_u16.to_le_bytes());
        assert_eq!(
            items(&kernel, &layout(root, 1), &mut memory).1,
            Err("it returned 3".to_owned())
        );
        memory.write(root, &2_u16.to_le_bytes());
        memory.write(root + 144 + 8, &0_u64.to_le_bytes());
        assert_eq!(
            items(&kernel, &layout(root, 1), &mut memory).1,
            Err("it returned 4".to_owned())
        );
        assert_eq!(
            items(&kernel, &layout(root, 64), &mut memory).1,
            Err("it returned 2".to_owned())
        );
        assert_eq!(
            items(&kernel, &[], &mut memory).1,
            Err("it returned 1".to_owned())
        );
    }
}
