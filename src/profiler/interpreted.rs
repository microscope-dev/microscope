//! The Python frames the BPF engine reads at each sample
//! (`backend::bpf::sampler`), named and spliced into the samples' native
//! stacks.
//!
//! The program writes a thread's frames to the CPU's ring as the kernel
//! takes its sample, and the sample after them, so in each ring a record
//! of frames comes just before the sample of the same thread and CPU it
//! was read for. Their times may be in either order, as kernels prepare a
//! sample before or after they run its program, so the recorder pairs them
//! by ring order, not by time; a record whose sample never comes was the
//! sample's that the ring dropped, and goes with it.
//!
//! Each activation of the interpreter's loop runs a segment of the
//! thread's frames, the innermost activation the innermost segment, and
//! each segment goes just before its activation, as the debugger places
//! them (`backend::linux::interpreted`): where the activations and segments
//! differ in number, the frames follow the native ones, and the stack says
//! so. A frame's code object is read and proved as the record is
//! drained, while the program most likely still runs: the record waits
//! for the watermark to pass it before it is processed, by when the
//! program may have exited. One freed since, or whose program exited
//! before its record was drained, is unreadable.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use foldhash::HashMap;

use super::tasks::LiveStop;
use crate::backend::bpf::sampler::{self, Gil, NO_OFFSET, PyStack, StackEnd, V8End, V8Isolate};
use crate::capture::CaptureBuilder;
use crate::capture::records::{NONE, Zone};
use crate::debug_info::DebugInfo;
use crate::image::python::PythonLayout;
use crate::model::{Pid, Tid};
use crate::runtime_model::thread_ids::ThreadIds;
use crate::runtime_model::{
    self, InterpretedFrame, RuntimeImage, RuntimeModel, SampledCode, SampledCodeFrame, SampledFrame,
};
use crate::{LoadBias, LoadedModule, VirtualAddress};

/// How long past a record of frames the drain may still bring its sample:
/// a record older than the watermark by more found its sample dropped.
pub(super) const PAIRING_SLACK: u64 = 10_000_000;

/// The label a sample of a thread that holds or waits for its
/// interpreter's GIL carries, as task snapshots label threads.
const GIL: &str = "gil";

/// The label a sample of a thread that runs in a V8 isolate carries, its
/// value the isolate's number, as the debugger lists threads.
const ISOLATE: &str = "isolate";

/// A process's interpreter that the engine reads, and what its walker
/// reads it by.
struct Runtime {
    model: Arc<dyn RuntimeModel>,
    bias: LoadBias,
    kind: Kind,
}

/// Which walker reads a runtime, and by what.
enum Kind {
    /// CPython's: its layout, and where the process keeps its runtime.
    Python {
        layout: Result<Box<PythonLayout>, Arc<str>>,
        state: Option<VirtualAddress>,
    },

    /// V8's, which keeps its frames on the native stack: its layout, and
    /// why it does not say where threads keep their isolate, if it does
    /// not.
    V8 {
        layout: Result<sampler::RawV8Layout, String>,
        unplaced: Option<Arc<str>>,
    },
}

/// Where a process's code at an address is, as the recording maps it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeAt {
    /// Code the runtime generated, which no image holds.
    Generated,

    /// An image's code, or a copy of it, at this address of the image,
    /// whose runtime is the process's.
    Runtime(crate::ImageAddress),
}

/// A JavaScript frame the V8 walker read at a sample, named as it was
/// drained: where it is on the stack, which places it among the sample's
/// native frames, and the functions it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacedFrame {
    pub frame_pointer: u64,

    /// Its native frame's address: its instruction in the walk's first
    /// frame, and the byte before its return address in any other.
    pub address: VirtualAddress,
    pub functions: Vec<crate::RecordedFrame>,
}

/// What a process's modules said of its interpreter, when it had made
/// `mappings`.
struct Found {
    mappings: u64,
    runtime: Option<Runtime>,
}

/// One thread's latest span of one label value.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Span {
    value: Box<str>,
    start: i64,
    end: i64,
}

/// The numbers of one process's isolates, by when a sample first showed
/// each, as the debugger numbers them (`backend::linux::holdings`). An
/// isolate's address may be another's once it is disposed, and a thread
/// runs in none between its isolate's scopes, which samples see and stops
/// seldom do. Node runs each isolate on one thread for that thread's life,
/// so a thread seen at an address again is in the isolate it was, and a
/// thread that finds an address no thread seen there is in now is in a new
/// isolate. A number is forgotten once the threads seen in it exit.
#[derive(Debug, Default)]
struct IsolateNumbers {
    /// The last number given.
    last: u64,

    /// Each isolate's number, and the living threads seen in it.
    numbers: BTreeMap<u64, (u64, BTreeSet<Tid>)>,

    /// The isolate each thread was last seen in, if it was in one.
    held: BTreeMap<Tid, u64>,
}

impl IsolateNumbers {
    /// The number of the isolate at `address` that thread `tid` was seen
    /// in, or of none where `address` is 0; `None` too once numbers run
    /// out.
    fn number(&mut self, tid: Tid, address: u64) -> Option<u64> {
        if address == 0 {
            self.held.remove(&tid);
            return None;
        }
        let held = &self.held;
        let known = self.numbers.get_mut(&address).filter(|(_, holders)| {
            holders.contains(&tid)
                || holders
                    .iter()
                    .any(|holder| held.get(holder) == Some(&address))
        });
        let number = if let Some((number, holders)) = known {
            holders.insert(tid);
            *number
        } else {
            let number = self.last.checked_add(1)?;
            self.last = number;
            self.numbers
                .insert(address, (number, BTreeSet::from([tid])));
            number
        };
        self.held.insert(tid, address);
        Some(number)
    }

    /// Forgets thread `tid`, which exited, and the number of each isolate
    /// only it was seen in.
    fn exited(&mut self, tid: Tid) {
        self.held.remove(&tid);
        self.numbers.retain(|_, (_, holders)| {
            holders.remove(&tid);
            !holders.is_empty()
        });
    }
}

/// A record of frames, by its thread, CPU, and time.
pub(super) type RecordKey = (Tid, u32, u64);

/// What a sample's interpreted frames are.
#[derive(Debug, PartialEq, Eq)]
pub enum Sampled {
    /// Its process runs no interpreter the engine reads.
    None,

    /// Its process does, and the frames are unknown: their record was
    /// lost, or came before the process's layout or the thread's state was
    /// known.
    Unknown,

    /// Innermost first, each activation's outermost marked, and whether
    /// they end before the thread's outermost.
    Frames {
        frames: Vec<InterpretedFrame>,
        ended: bool,
    },

    /// Frames of a runtime that keeps them on the native stack, innermost
    /// first, each to go before its native frame; whether they end before
    /// the thread's outermost; and whether the walk stopped at a frame
    /// pointer it could not follow, as one a function that keeps no frame
    /// left in its register, so that frames of the runtime's outside it
    /// are unknown.
    Placed {
        frames: Vec<PlacedFrame>,
        ended: bool,
        unreadable: bool,
    },
}

/// The layout a walker reads a process's runtime by.
#[derive(Debug, Clone)]
pub enum Layout {
    Python(sampler::RawPyLayout),
    V8(sampler::RawV8Layout),
}

/// What a process's interpreter tells the program: the layout it reads
/// the process by, and where it begins each thread, each by the thread's
/// id as microscope sees it, its state, and the id its state names it by.
#[derive(Debug)]
pub struct Walked {
    pub pid: u32,

    /// Which program the process runs: how many times the recording saw
    /// it exec. A new program keeps the pid but not its layout.
    pub program: u64,
    pub layout: Result<Layout, String>,
    pub threads: Vec<(u32, u64, u64)>,
    pub gaps: Vec<Arc<str>>,
}

/// The engine's part of a recording.
pub(super) struct Interpreted {
    /// The identifiers of the output events, which every record of frames
    /// begins with.
    pub(super) outputs: Vec<u64>,
    found: BTreeMap<Pid, Found>,

    /// Each thread's open span of each label, by its row and the label.
    spans: BTreeMap<(u32, &'static str), Span>,
    isolates: BTreeMap<Pid, IsolateNumbers>,

    /// The frames of each record drained but not yet processed, read as
    /// it was drained.
    drained: BTreeMap<RecordKey, Drained>,

    /// What each JavaScript frame read in this batch of records was named,
    /// by everything the name and its proof read. A busy program's records
    /// name the same frames over and over, and records drained together
    /// are read together; the next batch reads them afresh.
    named: HashMap<(Pid, NamedFrame), Option<Vec<crate::RecordedFrame>>>,
}

/// Every field of a [`SampledCodeFrame`] its name is read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct NamedFrame {
    after_call: bool,
    generated: bool,
    function: u64,
    shared: u64,
    script: u64,
    script_id: u64,
    literal_id: i32,
    bytecode: u64,
    bytecode_offset: u64,
}

impl NamedFrame {
    const fn of(frame: &SampledCodeFrame) -> Self {
        Self {
            after_call: frame.after_call,
            generated: frame.generated,
            function: frame.function,
            shared: frame.shared,
            script: frame.script,
            script_id: frame.script_id,
            literal_id: frame.literal_id,
            bytecode: frame.bytecode,
            bytecode_offset: frame.bytecode_offset,
        }
    }
}

/// The frames of a record, read as it was drained.
enum Drained {
    /// Read whole.
    Read(Sampled),

    /// JavaScript frames, which the runtime `model` owns the code of: which
    /// are the program's is known only once the mappings in effect at
    /// their sample are, and a drain may hold mappings it has not yet
    /// processed.
    V8(DrainedV8, Arc<dyn RuntimeModel>),
}

impl Interpreted {
    pub(super) fn new(outputs: Vec<u64>) -> Self {
        Self {
            outputs,
            found: BTreeMap::new(),
            spans: BTreeMap::new(),
            isolates: BTreeMap::new(),
            drained: BTreeMap::new(),
            named: HashMap::default(),
        }
    }

    /// Begins a batch of records, whose frames are read afresh.
    pub(super) fn batch(&mut self) {
        self.named.clear();
    }

    /// Whether what process `pid`'s modules say of its interpreter must
    /// be read again, now that it has made `mappings` in all.
    pub(super) fn stale(&self, pid: Pid, mappings: u64) -> bool {
        self.found
            .get(&pid)
            .is_none_or(|found| found.mappings != mappings)
    }

    /// Reads what process `pid`'s `modules` say of its interpreter, once it
    /// has made `mappings`.
    pub(super) fn find(&mut self, pid: Pid, mappings: u64, modules: &[(LoadedModule, DebugInfo)]) {
        let runtime = modules.iter().find_map(|(loaded, info)| {
            // A copy of a module's code binds the runtime of the module.
            if loaded.window.is_some() {
                return None;
            }
            let image: Arc<dyn RuntimeImage + Send + Sync> = Arc::clone(&info.image) as _;
            if let Some(v8) = image.v8() {
                let model = runtime_model::detect(&image)
                    .into_iter()
                    .flatten()
                    .find(|model| model.owns_generated_code())?;
                let mut layout = match v8.layout {
                    Ok(layout) => sampler::v8_layout(&layout)
                        .map_err(|missing| format!("V8's layout has no {missing}")),
                    Err(why) => Err(why.to_string()),
                };
                let placed: Result<(), Arc<str>> =
                    match image.thread_local(crate::image::v8::CURRENT_ISOLATE) {
                        Some(Ok(place)) => layout.as_mut().map_or(Ok(()), |layout| {
                            layout
                                .place_isolate(place, loaded.load_bias)
                                .map_err(Arc::from)
                        }),
                        Some(Err(why)) => Err(format!(
                            "its variable of the current isolate is not found: {why}"
                        )
                        .into()),
                        None => Err("its image names no variable of the current isolate".into()),
                    };
                return Some(Runtime {
                    model,
                    bias: loaded.load_bias,
                    kind: Kind::V8 {
                        layout,
                        unplaced: placed
                            .err()
                            .map(|why| format!("samples have no isolate labels: {why}").into()),
                    },
                });
            }
            let python = image.python()?;
            let stop = LiveStop::memory(pid.get(), loaded.load_bias);
            let model = runtime_model::detect(&image)
                .into_iter()
                .flatten()
                .find(|model| model.walked_threads(&stop).is_some())?;
            Some(Runtime {
                model,
                bias: loaded.load_bias,
                kind: Kind::Python {
                    layout: python.layout.map(Box::new),
                    state: loaded.load_bias.load(python.runtime.start),
                },
            })
        });

        self.found.insert(pid, Found { mappings, runtime });
    }

    /// Whether a native frame of process `pid` that runs the function or
    /// symbol `name`, and called what runs `inner`, is an activation of
    /// its interpreter's loop.
    pub(super) fn activation(&self, pid: Pid, name: &str, inner: Option<&str>) -> bool {
        self.found
            .get(&pid)
            .and_then(|found| found.runtime.as_ref())
            .is_some_and(|runtime| runtime.model.activation(name, inner))
    }

    /// Forgets process `pid`, which exited.
    pub(super) fn forget(&mut self, pid: Pid) {
        self.found.remove(&pid);
        self.isolates.remove(&pid);
    }

    /// Forgets thread `tid` of process `pid`, which exited, as the isolate
    /// it was last seen in.
    pub(super) fn thread_exited(&mut self, pid: Pid, tid: Tid) {
        if let Some(numbers) = self.isolates.get_mut(&pid) {
            numbers.exited(tid);
        }
    }

    /// Reads the frames of `record`, of process `pid`, as it is drained,
    /// where `code` says what code is the runtime's. A record held back to
    /// a later drain was read in the first.
    pub(super) fn drained(
        &mut self,
        pid: Pid,
        key: RecordKey,
        record: &[u8],
        code: &dyn Fn(u64) -> Option<CodeAt>,
    ) {
        if self.drained.contains_key(&key) {
            return;
        }

        // A record lies four bytes into the sample that carries it, short
        // of its fields' alignment; a copy aligns them.
        let aligned = record.to_vec();
        let Some(runtime) = self
            .found
            .get(&pid)
            .and_then(|found| found.runtime.as_ref())
        else {
            return;
        };
        let stop = LiveStop::memory(pid.get(), runtime.bias);
        let model = runtime.model.as_ref();
        let drained = match runtime.kind {
            Kind::Python { .. } => Drained::Read(read_record(model, &stop, Some(&aligned))),
            Kind::V8 { .. } => {
                let named = &mut self.named;
                let mut name = |frame: &SampledCodeFrame| {
                    named
                        .entry((pid, NamedFrame::of(frame)))
                        .or_insert_with(|| model.sampled_code_frame(&stop, frame))
                        .clone()
                };
                v8_record(Some(&aligned))
                    .and_then(|stack| {
                        drain_v8(&stack, &|address| owner(model, code, address), &mut name)
                    })
                    .map_or(Drained::Read(Sampled::Unknown), |drained| {
                        Drained::V8(drained, Arc::clone(&runtime.model))
                    })
            }
        };
        self.drained.insert(key, drained);
    }

    /// Forgets the frames of a record that goes with no sample.
    pub(super) fn dropped(&mut self, key: RecordKey) {
        self.drained.remove(&key);
    }

    /// The interpreted frames of a sample of process `pid`, from the
    /// record of frames paired with it, if any, as they were read when
    /// it was drained, and kept where `code`, the mappings in effect at
    /// the sample, says their code is the runtime's.
    pub(super) fn paired(
        &mut self,
        pid: Pid,
        record: Option<(RecordKey, &[u8])>,
        code: &dyn Fn(u64) -> Option<CodeAt>,
    ) -> Sampled {
        match record.and_then(|(key, _)| self.drained.remove(&key)) {
            Some(Drained::Read(sampled)) => sampled,
            Some(Drained::V8(drained, model)) => {
                drained.settle(&|address| owner(model.as_ref(), code, address))
            }
            None => self.sampled(pid, record.map(|(_, bytes)| bytes), code),
        }
    }

    /// Whether the code at `address` of process `pid`, which `code` says
    /// where it is, is its V8's own: code V8 generated, or its builtins or
    /// a copy of them. Interpreted frames of V8's run only in such code, so
    /// a stack with none of it runs none.
    pub(super) fn runs_v8(
        &self,
        pid: Pid,
        code: &dyn Fn(u64) -> Option<CodeAt>,
        address: VirtualAddress,
    ) -> bool {
        self.found
            .get(&pid)
            .and_then(|found| found.runtime.as_ref())
            .filter(|runtime| matches!(runtime.kind, Kind::V8 { .. }))
            .is_some_and(|runtime| {
                matches!(
                    owner(runtime.model.as_ref(), code, address.get()),
                    Some(Owner::Generated | Owner::Builtins)
                )
            })
    }

    /// The interpreted frames of a sample of process `pid`, from the
    /// record of frames paired with it, if any, read now.
    fn sampled(
        &self,
        pid: Pid,
        record: Option<&[u8]>,
        code: &dyn Fn(u64) -> Option<CodeAt>,
    ) -> Sampled {
        let Some(runtime) = self
            .found
            .get(&pid)
            .and_then(|found| found.runtime.as_ref())
        else {
            return Sampled::None;
        };

        let stop = LiveStop::memory(pid.get(), runtime.bias);
        match runtime.kind {
            Kind::Python { .. } => read_record(runtime.model.as_ref(), &stop, record),
            Kind::V8 { .. } => read_v8_record(runtime.model.as_ref(), &stop, record, code),
        }
    }

    /// What process `pid`'s interpreter tells the program, read now, or
    /// `None` for a process that runs no interpreter the engine reads.
    /// The process runs its `program`th program.
    pub(super) fn walked(&self, pid: Pid, program: u64) -> Option<Walked> {
        let runtime = self.found.get(&pid)?.runtime.as_ref()?;
        let (layout, state) = match &runtime.kind {
            Kind::V8 { layout, unplaced } => {
                return Some(Walked {
                    pid: pid.get(),
                    program,
                    layout: layout.clone().map(Layout::V8),
                    threads: Vec::new(),
                    gaps: unplaced.iter().cloned().collect(),
                });
            }
            Kind::Python { layout, state } => (layout, *state),
        };
        let layout = sampler::process_layout(
            layout.as_ref().map(AsRef::as_ref).map_err(AsRef::as_ref),
            state,
            pid.get(),
        )
        .map(Layout::Python);
        let stop = LiveStop::for_threads(pid.get(), runtime.bias);
        let hosts = match ThreadIds::new(&stop) {
            Ok(hosts) => hosts,
            Err(problem) => {
                return Some(Walked {
                    pid: pid.get(),
                    program,
                    layout,
                    threads: Vec::new(),
                    gaps: vec![problem],
                });
            }
        };

        let mut walked = runtime.model.walked_threads(&stop)?;
        let threads = mapped_threads(&mut walked, &hosts);

        Some(Walked {
            pid: pid.get(),
            program,
            layout,
            threads,
            gaps: walked.gaps,
        })
    }

    /// Labels the thread whose row is `thread` with what it did with its
    /// GIL at a sample at `time` of `period`: each run of samples with one
    /// label is one span.
    pub(super) fn label(
        &mut self,
        builder: &mut CaptureBuilder,
        thread: u32,
        sample: (i64, u64),
        gil: Gil,
    ) {
        let value = match gil {
            Gil::Held => Some("held"),
            Gil::Waiting => Some("waiting"),
            Gil::Unknown | Gil::Released | Gil::Off => None,
        };
        self.extend(builder, (thread, GIL), value, sample);
    }

    /// Labels thread `tid` of process `pid`, whose row is `thread`, with
    /// the number of the isolate it ran in at a sample at `time` of
    /// `period`: none where it ran in none, or where the isolate was not
    /// read, which the recording says why of once.
    pub(super) fn isolate(
        &mut self,
        builder: &mut CaptureBuilder,
        (pid, tid, thread): (Pid, Tid, u32),
        sample: (i64, u64),
        isolate: V8Isolate,
    ) {
        let number = match isolate {
            V8Isolate::Read(address) => self.isolates.entry(pid).or_default().number(tid, address),
            V8Isolate::NotRead | V8Isolate::Unreadable => None,
        };
        let value = number.map(|number| number.to_string());
        self.extend(builder, (thread, ISOLATE), value.as_deref(), sample);
    }

    /// Extends the span of label `key` on the thread whose row is
    /// `thread` with a sample at `time` of `period` whose value is `value`,
    /// or ends it, recording it, where the value differs or the samples do
    /// not meet.
    fn extend(
        &mut self,
        builder: &mut CaptureBuilder,
        (thread, key): (u32, &'static str),
        value: Option<&str>,
        (time, period): (i64, u64),
    ) {
        let start = time.saturating_sub(period.cast_signed());
        let span = self.spans.get_mut(&(thread, key));

        match (span, value) {
            // A sample takes up where the last of its value left off.
            (Some(span), Some(value)) if *span.value == *value && span.end >= start => {
                span.end = time;
            }
            (_, value) => {
                if let Some(span) = self.spans.remove(&(thread, key)) {
                    record_span(builder, (thread, key), &span);
                }

                if let Some(value) = value {
                    self.spans.insert(
                        (thread, key),
                        Span {
                            value: value.into(),
                            start,
                            end: time,
                        },
                    );
                }
            }
        }
    }

    /// Records each thread's open span in `builder`, as of where it last
    /// reached.
    pub(super) fn close(&self, builder: &mut CaptureBuilder) {
        for (&key, span) in &self.spans {
            record_span(builder, key, span);
        }
    }
}

/// Records a span of label `key` on the thread whose row is `thread`.
fn record_span(builder: &mut CaptureBuilder, (thread, key): (u32, &'static str), span: &Span) {
    let site = builder.site(key, "", 0, NONE, microscope_probes::abi::SITE_LABEL);
    let value = builder.string(&span.value);
    builder.zone(
        thread,
        Zone {
            start: span.start.into(),
            end: span.end.into(),
            site: site.into(),
            flags: 0.into(),
            value: value.into(),
            reserved: 0.into(),
        },
    );
}

/// The interpreted frames a record of the walker's holds, if any, named
/// and proved by `model` through `stop`, the process's memory read now.
pub fn read_record(
    model: &dyn RuntimeModel,
    stop: &LiveStop<'_>,
    record: Option<&[u8]>,
) -> Sampled {
    let Some(stack) = record.and_then(|bytes| sampler::decode(bytes).ok()) else {
        return Sampled::Unknown;
    };

    let ended = match stack.end {
        StackEnd::NoLayout | StackEnd::NoThread | StackEnd::Stale => return Sampled::Unknown,
        StackEnd::Complete => false,
        StackEnd::Bounded | StackEnd::Unreadable | StackEnd::BeforeBase => true,
    };

    let (frames, end) = model.sampled_frames(stop, &sampled_frames(&stack));
    Sampled::Frames {
        frames,
        ended: ended || end.is_some(),
    }
}

/// Whose code a JavaScript frame runs, as a recording's mappings say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Owner {
    /// Code V8 generated, whose frames are compiled.
    Generated,

    /// V8's builtins, or a copy of them, whose frames are its interpreter's
    /// or its own.
    Builtins,

    /// None of the mappings yet says: a drain may hold a mapping it has not
    /// yet processed, and the code may also be no runtime's.
    Unknown,
}

/// Whose code is at `address`, which `code` says where it is, of the
/// runtime `model`: `None` for another module's.
fn owner(
    model: &dyn RuntimeModel,
    code: &dyn Fn(u64) -> Option<CodeAt>,
    address: u64,
) -> Option<Owner> {
    match code(address) {
        Some(CodeAt::Generated) => Some(Owner::Generated),
        Some(CodeAt::Runtime(image)) if model.owns_code(image) => Some(Owner::Builtins),
        Some(CodeAt::Runtime(_)) => None,
        None => Some(Owner::Unknown),
    }
}

/// A JavaScript frame of a record, read as the record was drained.
#[derive(Debug)]
struct DrainedFrame {
    frame_pointer: u64,
    address: VirtualAddress,

    /// Whether its instruction is the return address of a call.
    after_call: bool,
    owner: Owner,

    /// Its functions, read as the frame of each code it may run: compiled
    /// code; the interpreter's, with no call seen made from it; and the
    /// interpreter's, under a call it made. `None` for one it cannot be.
    compiled: Option<Vec<crate::RecordedFrame>>,
    innermost: Option<Vec<crate::RecordedFrame>>,
    called: Option<Vec<crate::RecordedFrame>>,
}

/// The JavaScript frames of a record of the V8 walker's, read as it was
/// drained: their functions are read then, and which of the frames are
/// the program's, and how, is settled once the mappings in effect at
/// their sample are known.
#[derive(Debug)]
pub(super) struct DrainedV8 {
    frames: Vec<DrainedFrame>,
    ended: bool,
    unreadable: bool,
}

/// Reads the JavaScript frames of a V8 walker's `stack` now, each whose
/// code `owner` says may be the runtime's named by `name` as each code it
/// may run would name it.
///
/// A sample may land in a bytecode handler that keeps a frame of its own,
/// which the walk cannot tell from a call the frame made, and the
/// interpreter writes its offset to the frame only before a call: a
/// frame's place is known only once a frame of the program's it called
/// is, inside it. Where that waits on whose code an inner frame runs, the
/// frame is read both ways. A model names a frame or not by the function
/// it ran, not by the code it ran.
fn drain_v8(
    stack: &sampler::V8Stack<'_>,
    owner: &dyn Fn(u64) -> Option<Owner>,
    name: &mut dyn FnMut(&SampledCodeFrame) -> Option<Vec<crate::RecordedFrame>>,
) -> Option<DrainedV8> {
    let (ended, unreadable) = match stack.end {
        V8End::NoLayout => return None,
        V8End::Complete => (false, false),
        V8End::Unreadable => (false, true),
        V8End::Bounded => (true, false),
    };
    // Whether a frame inside this one is the program's: `None` where that
    // waits on whose code one runs.
    let mut called = Some(false);
    let mut frames = Vec::with_capacity(stack.frames.len());
    for sampled in stack.frames {
        let after_call = sampled.at_instruction == 0;
        let address = if after_call {
            sampled.pc.wrapping_sub(1)
        } else {
            sampled.pc
        };
        let Some(owner) = owner(address) else {
            continue;
        };
        let frame = |generated, after_call| SampledCodeFrame {
            frame_pointer: sampled.fp,
            instruction: VirtualAddress::new(sampled.pc),
            after_call,
            generated,
            function: sampled.function,
            shared: sampled.shared,
            script: sampled.script,
            script_id: sampled.script_id,
            literal_id: sampled.literal_id,
            bytecode: sampled.bytecode,
            bytecode_offset: sampled.bytecode_offset,
        };
        let interpreted = owner != Owner::Generated;
        let mut read = |wanted: bool, generated, after_call| {
            wanted
                .then(|| name(&frame(generated, after_call)))
                .flatten()
        };
        let compiled = read(owner != Owner::Builtins, true, false);
        let innermost = read(
            interpreted && !(after_call && called == Some(true)),
            false,
            false,
        );
        let under_call = read(
            interpreted && after_call && called != Some(false),
            false,
            true,
        );
        if compiled.is_none() && innermost.is_none() && under_call.is_none() {
            continue;
        }
        called = match (owner, called) {
            (Owner::Unknown, Some(false)) => None,
            (Owner::Unknown, called) => called,
            (Owner::Generated | Owner::Builtins, _) => Some(true),
        };
        frames.push(DrainedFrame {
            frame_pointer: sampled.fp,
            address: VirtualAddress::new(address),
            after_call,
            owner,
            compiled,
            innermost,
            called: under_call,
        });
    }
    Some(DrainedV8 {
        frames,
        ended,
        unreadable,
    })
}

impl DrainedV8 {
    /// The frames whose code `owner`, the mappings in effect at their
    /// sample, says is the runtime's, each named as the code it runs.
    fn settle(self, owner: &dyn Fn(u64) -> Option<Owner>) -> Sampled {
        let mut called = false;
        let mut frames = Vec::with_capacity(self.frames.len());
        for frame in self.frames {
            let owner = match frame.owner {
                Owner::Unknown => owner(frame.address.get()),
                known => Some(known),
            };
            let functions = match owner {
                None | Some(Owner::Unknown) => continue,
                Some(Owner::Generated) => frame.compiled,
                Some(Owner::Builtins) if frame.after_call && called => frame.called,
                Some(Owner::Builtins) => frame.innermost,
            };
            // The drain read each way the frame could turn out.
            debug_assert!(functions.is_some(), "a frame not read as it settled");
            let Some(functions) = functions else {
                continue;
            };
            called = true;
            frames.push(PlacedFrame {
                frame_pointer: frame.frame_pointer,
                address: frame.address,
                functions,
            });
        }
        Sampled::Placed {
            frames,
            ended: self.ended,
            unreadable: self.unreadable,
        }
    }
}

/// The JavaScript frames a record of the V8 walker's holds, read now by
/// `model` through `stop`, the process's memory, and not yet settled; or
/// `None` where they are unknown.
fn drain_v8_record(
    model: &dyn RuntimeModel,
    stop: &LiveStop<'_>,
    record: Option<&[u8]>,
    code: &dyn Fn(u64) -> Option<CodeAt>,
) -> Option<DrainedV8> {
    let stack = v8_record(record)?;
    drain_v8(
        &stack,
        &|address| owner(model, code, address),
        &mut |frame| model.sampled_code_frame(stop, frame),
    )
}

/// The V8 walker's stack `record` holds, if it is one.
fn v8_record(record: Option<&[u8]>) -> Option<sampler::V8Stack<'_>> {
    record
        .filter(|bytes| sampler::is_v8(bytes))
        .and_then(|bytes| sampler::decode_v8(bytes).ok())
}

/// The JavaScript frames a record of the V8 walker's holds, if any, each
/// named and proved by `model` through `stop`, the process's memory read
/// now, where `code` says its code is the runtime's.
pub fn read_v8_record(
    model: &dyn RuntimeModel,
    stop: &LiveStop<'_>,
    record: Option<&[u8]>,
    code: &dyn Fn(u64) -> Option<CodeAt>,
) -> Sampled {
    drain_v8_record(model, stop, record, code).map_or(Sampled::Unknown, |drained| {
        drained.settle(&|address| owner(model, code, address))
    })
}

/// Where a sample's JavaScript frames leave its stack short, given whether
/// its native frames run V8's own code (`runs_v8`): unknown frames, or a
/// walk that stopped at a frame it could not follow, leave out frames only
/// where V8's code runs, as no thread runs JavaScript in none.
pub(super) fn v8_cut(
    sampled: &Sampled,
    runs_v8: bool,
) -> Option<crate::capture::records::Truncation> {
    use crate::capture::records::Truncation;
    match *sampled {
        Sampled::Unknown => runs_v8.then_some(Truncation::InterpretedUnknown),
        Sampled::Placed {
            ended, unreadable, ..
        } => (ended || (unreadable && runs_v8)).then_some(Truncation::InterpretedEnded),
        Sampled::None | Sampled::Frames { .. } => None,
    }
}

/// Where each of a walker's frames goes among a sample's `natives`, each
/// native frame's address and frame pointer, innermost first: before the
/// index of the native frame it is, or at their end where it is none of
/// them, as one past where the native stack ended. `frames` are each
/// walked frame's frame pointer and native address, innermost first. A
/// frame is the first native frame not yet matched with its address whose
/// frame pointer, where known, is its own: several frames may run the same
/// code, as nested calls of interpreted functions do.
pub fn positions(
    natives: &[(VirtualAddress, Option<u64>)],
    frames: &[(u64, VirtualAddress)],
) -> Vec<usize> {
    let mut next = 0;
    frames
        .iter()
        .map(|&(fp, address)| {
            let found = natives
                .iter()
                .enumerate()
                .skip(next)
                .find(|(_, (native, pointer))| {
                    *native == address && pointer.is_none_or(|pointer| pointer == fp)
                })
                .map(|(index, _)| index);
            found.map_or(natives.len(), |index| {
                next = index + 1;
                index
            })
        })
        .collect()
}

/// The frames of a record, as the runtime model reads them.
fn sampled_frames(stack: &PyStack<'_>) -> Vec<SampledFrame> {
    stack
        .frames
        .iter()
        .map(|frame| SampledFrame {
            code: VirtualAddress::new(frame.code),
            proof: SampledCode {
                qualname: frame.qualname,
                linetable: frame.linetable,
                first_line: frame.first_line,
            },
            offset: (frame.offset != NO_OFFSET).then_some(i64::from(frame.offset)),
            owner: frame.owner,
        })
        .collect()
}

/// Keep examples and a total count without allocating one string for
/// each unsupported state in a large interpreter catalog.
struct MappingGaps<'a> {
    gaps: &'a mut Vec<Arc<str>>,
    failed: usize,
}

impl MappingGaps<'_> {
    const EXAMPLES: usize = 8;

    fn push(&mut self, why: impl FnOnce() -> Arc<str>) {
        self.failed += 1;
        if self.failed <= Self::EXAMPLES {
            self.gaps.push(why());
        }
    }

    fn finish(self) {
        if self.failed > Self::EXAMPLES {
            self.gaps.push(
                format!(
                    "{} Python thread mapping problems; {} additional details omitted",
                    self.failed,
                    self.failed - Self::EXAMPLES
                )
                .into(),
            );
        }
    }
}

/// Maps each state to one proven Linux thread. The kernel map holds one
/// state per host thread, so multiple interpreter states are ambiguous.
fn mapped_threads(
    walked: &mut runtime_model::Partial<Vec<runtime_model::WalkedThread>>,
    hosts: &ThreadIds,
) -> Vec<(u32, u64, u64)> {
    let mut gaps = MappingGaps {
        gaps: &mut walked.gaps,
        failed: 0,
    };
    let mut mapped = BTreeMap::<u32, Option<(u64, u64)>>::new();

    for thread in &walked.value {
        let host = match hosts.lookup(thread.native) {
            Ok(host) => host,
            Err(problem) => {
                gaps.push(|| problem.to_string().into());
                continue;
            }
        };
        let Some(kernel_id) = i32::try_from(host.get()).ok().filter(|host| *host > 0) else {
            gaps.push(|| format!("debugger thread {host} is outside Linux's TID range").into());
            continue;
        };
        let host = kernel_id.cast_unsigned();
        match mapped.entry(host) {
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(Some((thread.state.get(), thread.native)));
            }
            std::collections::btree_map::Entry::Occupied(mut slot) => {
                if slot.get().is_some() {
                    gaps.push(|| format!("debugger thread {host} has multiple Python thread states; the BPF engine cannot select one").into());
                }
                slot.insert(None);
            }
        }
    }
    gaps.finish();
    mapped
        .into_iter()
        .filter_map(|(host, state)| state.map(|(state, native)| (host, state, native)))
        .collect()
}

/// A stack's frames with the interpreted ones spliced in, innermost first:
/// each segment of `segments` just before the activation of the
/// interpreter's loop that runs it, the innermost segment before the
/// innermost activation, where `native` marks each frame that is one.
/// `None` where activations and segments differ in number, as no segment's
/// place is then known.
pub(super) fn splice(native: &[(u32, bool)], segments: &[Vec<u32>]) -> Option<Vec<u32>> {
    let activations = native.iter().filter(|(_, activation)| *activation).count();
    if activations != segments.len() {
        return None;
    }

    let mut spliced =
        Vec::with_capacity(native.len() + segments.iter().map(Vec::len).sum::<usize>());
    let mut segments = segments.iter();

    for &(frame, activation) in native {
        if activation {
            spliced.extend(segments.next().into_iter().flatten());
        }

        spliced.push(frame);
    }

    Some(spliced)
}

/// Whether interpreted frames that cannot be known leave a stack of
/// `native` frames short: only where an activation of the interpreter's
/// loop could run them. A thread that is in no activation, as a library's
/// worker thread is, runs no interpreted frames.
pub(super) fn unknown_cuts(native: &[(u32, bool)]) -> bool {
    native.iter().any(|&(_, activation)| activation)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each walked frame goes before the native frame it is, in order,
    /// matched by address and by frame pointer where that is known, and
    /// one that is no native frame goes at the end.
    #[test]
    fn walked_frames_go_before_their_native_frames_in_order() {
        let at = VirtualAddress::new;
        let natives = [
            (at(0x10), Some(0x100)),
            (at(0x20), None),
            (at(0x20), None),
            (at(0x30), Some(0x400)),
        ];
        assert_eq!(
            positions(
                &natives,
                &[(0x200, at(0x20)), (0x300, at(0x20)), (0x500, at(0x40))]
            ),
            [1, 2, 4]
        );
        // A frame pointer that is known must agree.
        assert_eq!(positions(&natives, &[(0x999, at(0x30))]), [4]);
        assert_eq!(positions(&natives, &[(0x400, at(0x30))]), [3]);
    }

    #[test]
    fn only_a_stack_in_the_loop_can_lack_interpreted_frames() {
        assert!(unknown_cuts(&[(1, false), (2, true), (3, false)]));
        assert!(!unknown_cuts(&[(1, false), (3, false)]));
        assert!(!unknown_cuts(&[]));
    }

    /// Each segment goes before its activation, innermost with innermost;
    /// where the two differ in number, nothing is placed.
    #[test]
    fn segments_go_before_their_activations_in_order() {
        let native = [(1, false), (2, true), (3, false), (4, true), (5, false)];
        let segments = [vec![10, 11], vec![20]];
        assert_eq!(
            splice(&native, &segments),
            Some(vec![1, 10, 11, 2, 3, 20, 4, 5])
        );
        assert_eq!(splice(&native, &segments[..1]), None);
        assert_eq!(splice(&[(1, false)], &[]), Some(vec![1]));
        assert_eq!(splice(&[(1, true)], &[vec![]]), Some(vec![1]));
    }

    /// A run of samples with one label is one span, from the first's
    /// period to the last; a change of value, or a sample with none, or a
    /// run that lapses, begins another.
    /// Isolates are numbered as samples first show them, and a number
    /// lasts while a thread last seen in its isolate lives: a thread that
    /// moves on, or exits, frees it, and an isolate at its address then
    /// gets a new number. A thread in no isolate has none.
    #[test]
    fn isolates_are_numbered_by_first_sight_while_a_thread_holds_them() {
        let mut numbers = IsolateNumbers::default();
        let (main, worker, helper) = (Tid::new(10), Tid::new(11), Tid::new(12));
        assert_eq!(numbers.number(main, 0xa000), Some(1));
        assert_eq!(numbers.number(main, 0xa000), Some(1));
        assert_eq!(numbers.number(worker, 0xb000), Some(2));
        assert_eq!(numbers.number(helper, 0xa000), Some(1));
        assert_eq!(numbers.number(helper, 0), None);
        assert_eq!(numbers.number(main, 0xa000), Some(1));

        // A thread between its isolate's scopes is in none, and back in
        // its own isolate after them, though no other thread is in it.
        assert_eq!(numbers.number(worker, 0), None);
        assert_eq!(numbers.number(worker, 0xb000), Some(2));

        numbers.exited(worker);
        assert_eq!(numbers.number(helper, 0xb000), Some(3));

        // A thread finds the address of an isolate whose threads live but
        // are not in it: a new isolate took the disposed one's address.
        assert_eq!(numbers.number(helper, 0), None);
        let late = Tid::new(14);
        assert_eq!(numbers.number(late, 0xb000), Some(4));
        assert_eq!(numbers.number(helper, 0xb000), Some(4));

        // Of the threads seen in the main isolate, one exited and the
        // other is in another: a thread at its address is in a new one.
        numbers.exited(main);
        assert_eq!(numbers.number(Tid::new(13), 0xa000), Some(5));
    }

    /// A walked frame at `pc` of the function whose shared information is
    /// at `shared`, at its instruction or after a call.
    fn walked(pc: u64, shared: u64, at_instruction: bool) -> sampler::RawV8Frame {
        sampler::RawV8Frame {
            fp: pc << 4,
            pc,
            function: shared + 1,
            shared,
            script: 0x50,
            script_id: 3,
            bytecode: 0,
            bytecode_offset: 0,
            literal_id: 1,
            at_instruction: u32::from(at_instruction),
        }
    }

    /// Each placed frame's name, which `named` gives it.
    fn names(sampled: &Sampled) -> Vec<String> {
        let Sampled::Placed { frames, .. } = sampled else {
            return vec![format!("{sampled:?}")];
        };
        frames
            .iter()
            .flat_map(|frame| frame.functions.iter())
            .map(|function| function.function.to_string())
            .collect()
    }

    /// Names a frame by its function and how it was read, as compiled
    /// code, the interpreter's innermost, or the interpreter's under a call.
    fn named(frame: &SampledCodeFrame) -> Vec<crate::RecordedFrame> {
        let how = match (frame.generated, frame.after_call) {
            (true, _) => "compiled",
            (false, false) => "innermost",
            (false, true) => "called",
        };
        vec![crate::RecordedFrame {
            function: format!("{:#x} {how}", frame.shared).into(),
            language: crate::SourceLanguage::JavaScript,
            place: None,
            generated: None,
            first_line: None,
            problem: None,
        }]
    }

    /// A frame in code a drain holds the mapping of but has not yet
    /// processed is read as the drain finds it and kept once the mapping
    /// in effect at its sample says the code is V8's, read as that code's
    /// frame; the frame outside it is then one it called. Where the
    /// mapping never says, the frame is dropped, and the frame outside it
    /// called none of the program's.
    #[test]
    fn a_frame_in_code_its_drain_has_not_mapped_is_kept_once_its_mapping_is() {
        let frames = [
            walked(0x1000, 0xa0, true),
            walked(0x2001, 0xb0, false),
            walked(0x3001, 0xc0, false),
        ];
        let stack = sampler::V8Stack {
            end: V8End::Complete,
            isolate: V8Isolate::NotRead,
            frames: &frames,
        };
        let drained_owner = |address| match address {
            0x1000 => Some(Owner::Unknown),
            0x2000 => Some(Owner::Builtins),
            _ => None,
        };
        let drain =
            || drain_v8(&stack, &drained_owner, &mut |frame| Some(named(frame))).expect("a layout");

        let mapped = drain().settle(&|address| match address {
            0x1000 => Some(Owner::Generated),
            other => drained_owner(other),
        });
        assert_eq!(names(&mapped), ["0xa0 compiled", "0xb0 called"]);

        let unmapped = drain().settle(&drained_owner);
        assert_eq!(names(&unmapped), ["0xb0 innermost"]);

        // Frames whose code the drain knows settle as they were read.
        let known = drain_v8(
            &stack,
            &|address| match address {
                0x1000 => Some(Owner::Builtins),
                other => drained_owner(other),
            },
            &mut |frame| Some(named(frame)),
        )
        .expect("a layout")
        .settle(&|_| None);
        assert_eq!(names(&known), ["0xa0 innermost", "0xb0 called"]);
    }

    /// Unknown JavaScript frames, or a walk that stopped at a frame
    /// pointer it could not follow, leave a stack short where its native
    /// frames run V8's code, and only there; a walk that filled its record
    /// does wherever it is.
    #[test]
    fn missing_javascript_frames_cut_a_stack_that_runs_v8() {
        use crate::capture::records::Truncation;
        let placed = |ended, unreadable| Sampled::Placed {
            frames: Vec::new(),
            ended,
            unreadable,
        };
        for (sampled, runs, cut) in [
            (Sampled::Unknown, true, Some(Truncation::InterpretedUnknown)),
            (Sampled::Unknown, false, None),
            (
                placed(false, true),
                true,
                Some(Truncation::InterpretedEnded),
            ),
            (placed(false, true), false, None),
            (
                placed(true, false),
                false,
                Some(Truncation::InterpretedEnded),
            ),
            (placed(false, false), true, None),
            (Sampled::None, true, None),
        ] {
            assert_eq!(v8_cut(&sampled, runs), cut, "{sampled:?}, runs V8: {runs}");
        }
    }

    /// Each run of samples in one isolate is one span of its label, which
    /// a sample in none, or one whose isolate was not read, ends.
    #[test]
    fn each_run_of_samples_in_one_isolate_is_one_span() {
        let mut interpreted = Interpreted::new(Vec::new());
        let mut builder = CaptureBuilder::new();
        let (pid, tid) = (Pid::new(10), Tid::new(10));
        let thread = builder.thread(pid, tid, 0);
        for (time, isolate) in [
            (10, V8Isolate::Read(0xa000)),
            (20, V8Isolate::Read(0xa000)),
            (30, V8Isolate::Unreadable),
            (40, V8Isolate::Read(0xa000)),
            (50, V8Isolate::Read(0)),
            (60, V8Isolate::Read(0xb000)),
            (70, V8Isolate::NotRead),
        ] {
            interpreted.isolate(&mut builder, (pid, tid, thread), (time, 10), isolate);
        }
        interpreted.close(&mut builder);
        let capture = builder.finish();
        let mut spans: Vec<(i64, i64, String)> = capture
            .zones
            .iter()
            .map(|zone| {
                (
                    zone.start.get(),
                    zone.end.get(),
                    capture.string(zone.value.get()).to_owned(),
                )
            })
            .collect();
        spans.sort();
        assert_eq!(
            spans,
            [
                (0, 20, "1".to_owned()),
                (30, 40, "1".to_owned()),
                (50, 60, "2".to_owned()),
            ]
        );
    }

    #[test]
    fn each_run_of_one_gil_label_is_one_span() {
        let mut interpreted = Interpreted::new(Vec::new());
        let mut builder = CaptureBuilder::new();
        let thread = builder.thread(Pid::new(1), crate::model::Tid::new(1), 0);

        for (time, gil) in [
            (10, Gil::Held),
            (20, Gil::Held),
            (30, Gil::Waiting),
            (40, Gil::Released),
            (50, Gil::Held),
            (90, Gil::Held),
        ] {
            interpreted.label(&mut builder, thread, (time, 10), gil);
        }

        interpreted.close(&mut builder);
        let capture = builder.finish();
        let mut spans: Vec<(i64, i64, String)> = capture
            .zones
            .iter()
            .map(|zone| {
                (
                    zone.start.get(),
                    zone.end.get(),
                    capture.string(zone.value.get()).to_owned(),
                )
            })
            .collect();
        spans.sort();
        assert_eq!(
            spans,
            [
                (0, 20, "held".to_owned()),
                (20, 30, "waiting".to_owned()),
                (40, 50, "held".to_owned()),
                (80, 90, "held".to_owned()),
            ]
        );
    }

    #[test]
    fn missing_host_identity_never_assigns_python_state_by_numeric_equality() {
        let mut walked = runtime_model::Partial {
            value: vec![runtime_model::WalkedThread {
                native: 7,
                state: VirtualAddress::new(0x1234),
            }],
            gaps: Vec::new(),
        };
        let hosts = ThreadIds::read(Vec::new(), |_| Vec::new()).unwrap();
        let mapped = mapped_threads(&mut walked, &hosts);
        assert!(
            mapped.is_empty(),
            "unproven state was assigned to host thread 7: {mapped:?}"
        );
        assert!(
            !walked.gaps.is_empty(),
            "missing identity evidence is visible"
        );
    }

    #[test]
    fn proven_namespace_ids_preserve_good_states_and_report_missing_ones() {
        let host = crate::ThreadId::new(5000);
        let collision = crate::ThreadId::new(7);
        let hosts = ThreadIds::read(vec![host, collision], |_| {
            vec![
                (host, runtime_model::TargetThreadId::new(7).unwrap()),
                (collision, runtime_model::TargetThreadId::new(8).unwrap()),
            ]
        })
        .unwrap();
        let mut walked = runtime_model::Partial {
            value: [(7, 0x1000), (8, 0x2000), (9, 0x3000)]
                .map(|(native, state)| runtime_model::WalkedThread {
                    native,
                    state: VirtualAddress::new(state),
                })
                .to_vec(),
            gaps: vec!["existing runtime gap".into()],
        };
        assert_eq!(
            mapped_threads(&mut walked, &hosts),
            [(7, 0x2000, 8), (5000, 0x1000, 7)]
        );
        assert_eq!(walked.gaps.len(), 2);
        assert_eq!(&*walked.gaps[0], "existing runtime gap");
        assert!(walked.gaps[1].contains('9') && walked.gaps[1].contains("no proven"));
    }

    #[test]
    fn ambiguous_namespace_ids_and_host_ids_outside_the_kernel_range_are_visible() {
        for ambiguous in [false, true] {
            let first = crate::ThreadId::new(if ambiguous {
                5000
            } else {
                u64::from(u32::MAX) + 1
            });
            let second = crate::ThreadId::new(5001);
            let hosts = ThreadIds::read(vec![first, second], |_| {
                let mut pairs = vec![(first, runtime_model::TargetThreadId::new(7).unwrap())];
                if ambiguous {
                    pairs.push((second, runtime_model::TargetThreadId::new(7).unwrap()));
                }
                pairs
            })
            .unwrap();
            let mut walked = runtime_model::Partial {
                value: vec![runtime_model::WalkedThread {
                    native: 7,
                    state: VirtualAddress::new(0x1000),
                }],
                gaps: Vec::new(),
            };
            assert!(mapped_threads(&mut walked, &hosts).is_empty());
            assert_eq!(walked.gaps.len(), 1);
            assert!(walked.gaps[0].contains(if ambiguous { "ambiguous" } else { "TID range" }));
        }
    }

    #[test]
    fn mapping_diagnostics_are_bounded_without_losing_later_proven_states() {
        let host = crate::ThreadId::new(5000);
        let hosts = ThreadIds::read(vec![host], |_| {
            vec![(host, runtime_model::TargetThreadId::new(7).unwrap())]
        })
        .unwrap();
        let mut walked = runtime_model::Partial {
            value: (10..10010)
                .map(|native| runtime_model::WalkedThread {
                    native,
                    state: VirtualAddress::new(native * 8),
                })
                .collect::<Vec<_>>(),
            gaps: vec!["existing runtime gap".into()],
        };
        walked.value.push(runtime_model::WalkedThread {
            native: 7,
            state: VirtualAddress::new(0x7777),
        });
        assert_eq!(mapped_threads(&mut walked, &hosts), [(5000, 0x7777, 7)]);
        assert!(
            walked.gaps.len() <= 10,
            "{} mapping diagnostics",
            walked.gaps.len()
        );
        assert!(walked.gaps.last().unwrap().contains("10000"));
        assert_eq!(&*walked.gaps[0], "existing runtime gap");
    }

    #[test]
    fn multiple_python_states_on_one_host_thread_are_not_chosen_arbitrarily() {
        let mut walked = runtime_model::Partial {
            value: vec![
                runtime_model::WalkedThread {
                    native: 7,
                    state: VirtualAddress::new(0x1000),
                },
                runtime_model::WalkedThread {
                    native: 7,
                    state: VirtualAddress::new(0x2000),
                },
            ],
            gaps: Vec::new(),
        };
        let hosts = ThreadIds::read(vec![crate::ThreadId::new(5000)], |_| {
            vec![(
                crate::ThreadId::new(5000),
                runtime_model::TargetThreadId::new(7).unwrap(),
            )]
        })
        .unwrap();
        let mapped = mapped_threads(&mut walked, &hosts);
        assert!(
            mapped.is_empty(),
            "one kernel key cannot select between interpreter states: {mapped:?}"
        );
        assert!(!walked.gaps.is_empty());
    }
}
