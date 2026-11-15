use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use foldhash::HashMap;

use super::tasks::LiveStop;
use crate::backend::bpf::sampler::{self, Gil, NO_OFFSET, PyStack, StackEnd, V8End, V8Isolate};
use crate::backend::perf::decode;
use crate::capture::CaptureBuilder;
use crate::capture::records::{NONE, Zone};
use crate::debug_info::DebugInfo;
use crate::image::python::{PythonLayout, PythonRuntime};
use crate::model::{Pid, Tid};
use crate::runtime_model::thread_ids::ThreadIds;
use crate::runtime_model::{
    self, InterpretedFrame, RuntimeImage, RuntimeModel, SampledCode, SampledCodeFrame, SampledFrame,
};
use crate::{LoadBias, LoadedModule, VirtualAddress};

pub(super) const PAIRING_SLACK: u64 = 10_000_000;

const GIL: &str = "gil";

const ISOLATE: &str = "isolate";

struct Runtime {
    model: Arc<dyn RuntimeModel>,
    bias: LoadBias,
    kind: Kind,
}

enum Kind {
    Python {
        layout: Result<Box<PythonLayout>, Arc<str>>,
        state: Option<VirtualAddress>,
    },

    V8 {
        layout: Result<sampler::RawV8Layout, String>,
        unplaced: Option<Arc<str>>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeAt {
    Generated,

    Runtime(crate::ImageAddress),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacedFrame {
    pub frame_pointer: u64,

    pub address: VirtualAddress,
    pub functions: Vec<crate::RecordedFrame>,
}

struct Found {
    mappings: u64,
    runtime: Option<Runtime>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Span {
    value: Box<str>,
    start: i64,
    end: i64,
}

#[derive(Debug, Default)]
struct IsolateNumbers {
    last: u64,

    numbers: BTreeMap<u64, (u64, BTreeSet<Tid>)>,

    held: BTreeMap<Tid, u64>,
}

impl IsolateNumbers {
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

    fn exited(&mut self, tid: Tid) {
        self.held.remove(&tid);
        self.numbers.retain(|_, (_, holders)| {
            holders.remove(&tid);
            !holders.is_empty()
        });
    }
}

pub(super) type RecordKey = (Tid, u32, u64);

#[derive(Debug, PartialEq, Eq)]
pub enum Sampled {
    None,

    Unknown,

    Frames {
        frames: Vec<InterpretedFrame>,
        ended: bool,
    },

    Placed {
        frames: Vec<PlacedFrame>,
        ended: bool,
        unreadable: bool,
    },
}

#[derive(Debug, Clone)]
pub enum Layout {
    Python(sampler::RawPyLayout),
    V8(sampler::RawV8Layout),
}

#[derive(Debug, Clone)]
pub struct Walked {
    pub pid: u32,

    pub program: u64,
    pub layout: Result<Layout, String>,
    pub threads: Vec<(u32, u64, u64)>,
    pub gaps: Vec<Arc<str>>,
}

const NAMING_BLOCK: usize = 4096;

pub(super) struct Interpreted {
    pub(super) outputs: Vec<u64>,
    found: BTreeMap<Pid, Found>,

    spans: BTreeMap<(u32, &'static str), Span>,
    isolates: BTreeMap<Pid, IsolateNumbers>,

    drained: BTreeMap<RecordKey, Drained>,

    named: HashMap<(Pid, NamedFrame), Option<Vec<crate::RecordedFrame>>>,

    memory: HashMap<Pid, crate::runtime_model::StopMemory>,
    early: BTreeMap<Pid, (u64, Walked)>,
    fresh: Vec<Walked>,
    runtimes: HashMap<(std::path::PathBuf, u64), Option<Arc<PythonRuntime>>>,
}

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

enum Drained {
    Read(Sampled),

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
            memory: HashMap::default(),
            early: BTreeMap::new(),
            fresh: Vec::new(),
            runtimes: HashMap::default(),
        }
    }

    pub(super) fn mapped_early(&mut self, mmap: &decode::Mmap<'_>, program: u64) {
        if self.early.contains_key(&mmap.pid) {
            return;
        }

        let Some((file, bias)) = crate::backend::perf::identify(mmap) else {
            return;
        };
        let runtime = self
            .runtimes
            .entry((file, mmap.inode.unwrap_or(0)))
            .or_insert_with_key(|(file, _)| crate::debug_info::python_runtime(file).map(Arc::new));
        let Some(runtime) = runtime else {
            return;
        };

        let layout = sampler::process_layout(
            runtime.layout.as_ref().map_err(AsRef::as_ref),
            bias.load(runtime.runtime.start),
            mmap.pid.get(),
        )
        .map(Layout::Python);
        let walked = Walked {
            pid: mmap.pid.get(),
            program,
            layout,
            threads: Vec::new(),
            gaps: Vec::new(),
        };
        self.fresh.push(walked.clone());
        self.early.insert(mmap.pid, (mmap.time, walked));
    }

    pub(super) fn take_early(&mut self) -> Vec<Walked> {
        std::mem::take(&mut self.fresh)
    }

    pub(super) fn early_unknown(&self, known: impl Fn(Pid) -> bool) -> Vec<Walked> {
        self.early
            .iter()
            .filter(|&(&pid, _)| !known(pid))
            .map(|(_, (_, walked))| walked.clone())
            .collect()
    }

    pub(super) fn forget_early(&mut self, pid: Pid, time: u64) {
        if self
            .early
            .get(&pid)
            .is_some_and(|&(mapped, _)| mapped < time)
        {
            self.early.remove(&pid);
        }
    }

    pub(super) fn batch(&mut self) {
        self.named.clear();
        self.memory.clear();
    }

    pub(super) fn stale(&self, pid: Pid, mappings: u64) -> bool {
        self.found
            .get(&pid)
            .is_none_or(|found| found.mappings != mappings)
    }

    pub(super) fn find(&mut self, pid: Pid, mappings: u64, modules: &[(LoadedModule, DebugInfo)]) {
        let runtime = modules.iter().find_map(|(loaded, info)| {
            if loaded.window.is_some() {
                return None;
            }
            let image: Arc<dyn RuntimeImage + Send + Sync> = Arc::clone(&info.image) as _;
            if let Some((model, kind)) = v8_runtime(&image, Some(loaded.load_bias)) {
                return Some(Runtime {
                    model,
                    bias: loaded.load_bias,
                    kind,
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

    pub(super) fn activation(&self, pid: Pid, name: &str, inner: Option<&str>) -> bool {
        self.found
            .get(&pid)
            .and_then(|found| found.runtime.as_ref())
            .is_some_and(|runtime| runtime.model.activation(name, inner))
    }

    pub(super) fn forget(&mut self, pid: Pid) {
        self.found.remove(&pid);
        self.isolates.remove(&pid);
    }

    pub(super) fn thread_exited(&mut self, pid: Pid, tid: Tid) {
        if let Some(numbers) = self.isolates.get_mut(&pid) {
            numbers.exited(tid);
        }
    }

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
                let blocks = self
                    .memory
                    .entry(pid)
                    .or_insert_with(|| crate::runtime_model::StopMemory::with_block(NAMING_BLOCK));
                let stop = stop.through(blocks);
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

    pub(super) fn dropped(&mut self, key: RecordKey) {
        self.drained.remove(&key);
    }

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

    pub(super) fn walked(&self, pid: Pid, program: u64) -> Option<Walked> {
        let Some(runtime) = self
            .found
            .get(&pid)
            .and_then(|found| found.runtime.as_ref())
        else {
            return self.early.get(&pid).map(|(_, walked)| walked.clone());
        };
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

    pub(super) fn close(&self, builder: &mut CaptureBuilder) {
        for (&key, span) in &self.spans {
            record_span(builder, key, span);
        }
    }
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Owner {
    Generated,

    Builtins,

    Unknown,
}

fn v8_runtime(
    image: &Arc<dyn RuntimeImage + Send + Sync>,
    bias: Option<LoadBias>,
) -> Option<(Arc<dyn RuntimeModel>, Kind)> {
    let v8 = image.v8()?;
    let model = runtime_model::detect(image)
        .into_iter()
        .flatten()
        .find(|model| model.owns_generated_code())?;
    let mut layout = match v8.layout {
        Ok(layout) => {
            sampler::v8_layout(&layout).map_err(|missing| format!("V8's layout has no {missing}"))
        }
        Err(why) => Err(why.to_string()),
    };
    let placed: Result<(), Arc<str>> = match image.thread_local(crate::image::v8::CURRENT_ISOLATE) {
        Some(Ok(crate::ThreadLocal::Slot(_))) if bias.is_none() => {
            Err("its slot of the current isolate is not yet mapped".into())
        }
        Some(Ok(place)) => layout.as_mut().map_or(Ok(()), |layout| {
            layout
                .place_isolate(place, bias.unwrap_or_default())
                .map_err(Arc::from)
        }),
        Some(Err(why)) => {
            Err(format!("its variable of the current isolate is not found: {why}").into())
        }
        None => Err("its image names no variable of the current isolate".into()),
    };
    let unplaced = placed
        .err()
        .map(|why| format!("samples have no isolate labels: {why}").into());
    Some((model, Kind::V8 { layout, unplaced }))
}

pub(super) fn v8_layout_before(info: &DebugInfo) -> Option<sampler::RawV8Layout> {
    let image: Arc<dyn RuntimeImage + Send + Sync> = Arc::clone(&info.image) as _;
    match v8_runtime(&image, None)?.1 {
        Kind::V8 { layout, .. } => layout.ok(),
        Kind::Python { .. } => None,
    }
}

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

#[derive(Debug)]
struct DrainedFrame {
    frame_pointer: u64,
    address: VirtualAddress,

    after_call: bool,
    owner: Owner,

    compiled: Option<Vec<crate::RecordedFrame>>,
    innermost: Option<Vec<crate::RecordedFrame>>,
    called: Option<Vec<crate::RecordedFrame>>,
}

#[derive(Debug)]
pub(super) struct DrainedV8 {
    frames: Vec<DrainedFrame>,
    ended: bool,
    unreadable: bool,
}

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

fn v8_record(record: Option<&[u8]>) -> Option<sampler::V8Stack<'_>> {
    record
        .filter(|bytes| sampler::is_v8(bytes))
        .and_then(|bytes| sampler::decode_v8(bytes).ok())
}

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
            function: (frame.function != 0).then_some(frame.function),
        })
        .collect()
}

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

pub(super) fn unknown_cuts(native: &[(u32, bool)]) -> bool {
    native.iter().any(|&(_, activation)| activation)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(positions(&natives, &[(0x999, at(0x30))]), [4]);
        assert_eq!(positions(&natives, &[(0x400, at(0x30))]), [3]);
    }

    #[test]
    fn only_a_stack_in_the_loop_can_lack_interpreted_frames() {
        assert!(unknown_cuts(&[(1, false), (2, true), (3, false)]));
        assert!(!unknown_cuts(&[(1, false), (3, false)]));
        assert!(!unknown_cuts(&[]));
    }

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

        assert_eq!(numbers.number(worker, 0), None);
        assert_eq!(numbers.number(worker, 0xb000), Some(2));

        numbers.exited(worker);
        assert_eq!(numbers.number(helper, 0xb000), Some(3));

        assert_eq!(numbers.number(helper, 0), None);
        let late = Tid::new(14);
        assert_eq!(numbers.number(late, 0xb000), Some(4));
        assert_eq!(numbers.number(helper, 0xb000), Some(4));

        numbers.exited(main);
        assert_eq!(numbers.number(Tid::new(13), 0xa000), Some(5));
    }

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
            declared_in: None,
            problem: None,
        }]
    }

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
