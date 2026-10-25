//! Turns the records perf events write into a capture: it follows each
//! process's mappings and threads, unwinds each sample through the modules
//! mapped when it was taken, and symbolizes the frames once at the end.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use foldhash::HashMap;

use super::calls::Branch;
use super::collector::Found;
use super::frame_pointers::{CallSites, call_sites};
use super::interpreted::{self, Interpreted, Sampled, Walked};
use super::jit::{Jit, JitCode, Stale};
use super::probes::{self, Event, SiteRecord, Stacks};
use super::tasks::{InterpretedWait, Snapshot, WaitFrame};
use crate::backend::perf::{self, decode};
use crate::capture::builder::{InterpretedPlace, LineInput};
use crate::capture::records::{
    AccountKind, FrameKind, GapReason, Mapping, MarkerKind, NONE, Truncation, Zone, role_number,
};

use crate::capture::{Capture, CaptureBuilder};
use crate::debug_info::{DebugFileSearch, DebugInfo, gdb_jit};
use crate::model::{Pid, Tid};
use crate::unwind::UnwindModule;
use crate::unwind::sample::{FrameRules, SampledState, StackEnd, unwind_sample};
use crate::{
    CodeInstanceKind, CodeRole, ImageAddress, InlineFrameLookup, LoadBias, LoadedModule, ModuleId,
    ModuleImage, ModuleImageId,
};

/// The modules `process` maps, each once, with their debug information,
/// from `images`; a module whose debug information could not be read is
/// left out. A copy of a module's code, as V8's of its builtins, is a
/// module of its own beside the module.
fn modules_in(process: &ProcessState, images: &[Image]) -> Vec<(LoadedModule, DebugInfo)> {
    let mut seen = BTreeSet::new();
    process
        .mapped
        .iter()
        .filter(|mapped| {
            let window = mapped
                .loaded
                .window
                .map(|window| (window.start.get(), window.end.get()));
            seen.insert((mapped.image, window))
        })
        .filter_map(|mapped| {
            let info = images.get(mapped.image)?.info.clone()?;
            Some((mapped.loaded, info))
        })
        .collect()
}

/// Has `interpreted` read again what process `pid`'s modules say of its
/// interpreter, if they changed since.
fn find_interpreter(
    interpreted: &mut Interpreted,
    pid: Pid,
    process: &ProcessState,
    images: &[Image],
) {
    if interpreted.stale(pid, process.mappings) {
        interpreted.find(pid, process.mappings, &modules_in(process, images));
    }
}

/// Where process `pid`'s code at `address` is, for its runtime's walker:
/// code a runtime generated, or a V8 image's.
fn code_at(
    processes: &BTreeMap<Pid, ProcessState>,
    images: &[Image],
    pid: Pid,
    address: u64,
) -> Option<interpreted::CodeAt> {
    let mapped = processes
        .get(&pid)?
        .mapped
        .iter()
        .find(|mapped| mapped.start <= address && address < mapped.end)?;
    let image = images.get(mapped.image)?;
    if image.generated {
        return Some(interpreted::CodeAt::Generated);
    }
    let info = image.info.as_ref()?;
    crate::debug_info::v8::is_v8(&info.image).then(|| {
        interpreted::CodeAt::Runtime(
            mapped
                .loaded
                .load_bias
                .wrapping_unload(crate::VirtualAddress::new(address)),
        )
    })
}

/// A sample's native frames as a capture keeps them: each frame, with the
/// name of the interpreter's code it runs, if it runs the interpreter's;
/// and each frame's module and address, its key.
type NativeFrames = (Vec<(u32, Option<Arc<str>>)>, Vec<(u32, u64)>);

/// How a program's call sites keep the frame-pointer chain.
pub enum Sites {
    /// It is linked dynamically, so its code is not all in its file.
    Dynamic,

    /// Its own file's, with the function of its first call that does not.
    Static(CallSites, Option<String>),
}

/// A module file the recording mapped, loaded once for every mapping.
struct Image {
    /// `None` when its debug information could not be read: its frames
    /// are known by address only.
    info: Option<DebugInfo>,
    id: ModuleImageId,

    /// The capture's module.
    module: u32,

    /// Its instrumentation, if its file has any.
    probes: Option<Arc<probes::Table>>,

    /// Whether it is anonymous code a perf map or a JIT compiler's
    /// registered object names, which no file holds for a viewer to
    /// symbolize it from later: its frames are symbolized as they are
    /// recorded, and its frames outside every function it names are frames
    /// of no module.
    anonymous: bool,

    /// Whether it is code a runtime generated, as V8's compiled
    /// JavaScript, which the runtime's frames run.
    generated: bool,
}

/// One executable mapping of a process.
#[derive(Debug, Clone, Copy)]
struct Mapped {
    start: u64,
    end: u64,
    image: usize,
    loaded: LoadedModule,
}

/// Takes `start..end` from the modules `mapped` holds, as a new mapping
/// there replaces them: a module it covers part of keeps the rest, in
/// pieces of the same image and bias.
fn uncover(mapped: &mut Vec<Mapped>, start: u64, end: u64) {
    let mut kept = Vec::with_capacity(mapped.len() + 1);

    for piece in mapped.drain(..) {
        if piece.end <= start || piece.start >= end {
            kept.push(piece);
            continue;
        }

        if piece.start < start {
            kept.push(Mapped {
                end: start,
                ..piece
            });
        }

        if piece.end > end {
            kept.push(Mapped {
                start: end,
                ..piece
            });
        }
    }

    *mapped = kept;
}

#[derive(Debug, Default)]
struct ProcessState {
    mapped: Vec<Mapped>,

    /// How many mappings the process has made, which tells its modules
    /// apart from what they were, as their number may not.
    mappings: u64,
    threads: BTreeSet<Tid>,
    jit: Jit,

    /// Anonymous code mapped before any module that carries V8, which
    /// is V8's once one is mapped: mappings read from `/proc/PID/maps`
    /// come in address order, and V8's code range may lie below the
    /// module, as below Debian's `libnode`.
    before_v8: Vec<std::ops::Range<u64>>,
}

/// When one task snapshot held the program stopped, and the task row of
/// each task by where its runtime kept it.
struct Located {
    start: i64,
    end: i64,
    tasks: HashMap<u64, u32>,
}

/// The snapshots a recording keeps to claim samples by: records may come
/// a drain or two after the snapshot that follows them.
const LOCATED_KEPT: usize = 4;

/// A process's executable mappings, as `/proc/PID/maps` lists them.
type ProcessMappings = (Pid, Vec<perf::Mapped>);

/// Builds a capture from perf records as they are drained.
pub struct Processor {
    builder: CaptureBuilder,
    rules: FrameRules,
    search: DebugFileSearch,
    images: Vec<Image>,
    image_index: HashMap<(PathBuf, u64), usize>,

    /// Whether the vDSO's image has been read, and if it could be, the
    /// image and the address it links at.
    vdso_read: bool,
    vdso: Option<(usize, u64)>,
    processes: BTreeMap<Pid, ProcessState>,
    next_module: u32,
    limit: usize,

    /// Records held back for the next batch, past its watermark.
    carry: Vec<u8>,

    /// The ring heads that could not be trusted, as last counted.
    untrusted: u64,

    /// The latest time processed.
    latest: u64,
    first: Option<u64>,
    order: Vec<(u64, usize, usize)>,
    registers: Vec<(crate::unwind::DwarfRegister, u64)>,

    /// A breakpoint's instruction and the branch that makes its samples
    /// repeats, not calls.
    repeats: Option<(u64, Branch)>,
    chain: Vec<u64>,
    frames: Vec<u32>,

    /// Each thread off its CPU since a switch: its row, when it left, and
    /// whether it was preempted.
    switched_out: BTreeMap<Tid, (u32, u64, bool)>,

    /// Each gap in sampling, in time order and apart, and where the last
    /// one ended: a recording that ends paused ends there.
    gaps: Vec<(u64, u64)>,
    gaps_end: u64,

    /// The CPU time of this thread when the processor began, and what
    /// loading images has taken of it since.
    began: std::time::Duration,
    loading: std::time::Duration,

    /// The time every record up to has been processed.
    processed: u64,
    window: Option<Window>,

    /// Where the segment being recorded starts, and the counts there, once
    /// the recording has been cut into segments.
    segment: Option<(u64, BTreeMap<u32, u64>)>,

    /// The process task snapshots read, the last few snapshots, and each
    /// sample no snapshot after it has claimed yet whose code kept its task
    /// in a register: its thread, time, and the register's value.
    tasks_of: Option<Pid>,
    located: std::collections::VecDeque<Located>,
    unclaimed: Vec<(u32, i64, u64)>,

    /// Each process's modules with instrumentation, once mapped since it
    /// last executed a program, those the collector has not taken yet, and
    /// the processes that executed one since the collector last asked.
    probes_mapped: BTreeSet<(Pid, usize)>,
    probes_found: Vec<(Pid, Found)>,
    probes_executed: BTreeSet<Pid>,

    /// The mappings of each process read after a loss, by the time they
    /// were read, which take the place of the records lost once every
    /// record before that time is processed (see [`Processor::lost`]).
    lost_mappings: std::collections::VecDeque<(u64, Vec<ProcessMappings>)>,
    stacks: Stacks,
    triggered: bool,

    /// Whether frames are left for a viewer to symbolize.
    symbols_later: bool,

    /// The BPF engine's Python frames, when it runs, and the record of
    /// them paired with the sample being processed.
    interpreted: Option<Interpreted>,
    paired: Option<(interpreted::RecordKey, Vec<u8>)>,
}

/// A rolling window: how long it is, and its marks, each a moment and the
/// counts at it, oldest first. The oldest mark the window has passed is
/// its start: nothing before it is kept, and nothing it counted counts.
struct Window {
    length: u64,
    marks: std::collections::VecDeque<(u64, BTreeMap<u32, u64>)>,
}

impl Processor {
    pub fn new(search: DebugFileSearch, limit: usize) -> Self {
        Self {
            builder: CaptureBuilder::new(),
            rules: FrameRules::default(),
            search,
            images: Vec::new(),
            image_index: HashMap::default(),
            vdso_read: false,
            vdso: None,
            processes: BTreeMap::new(),
            next_module: 0,
            // A stack keeps at least its innermost frame.
            limit: limit.max(1),
            carry: Vec::new(),
            untrusted: 0,
            latest: 0,
            first: None,
            order: Vec::new(),
            registers: Vec::with_capacity(decode::DWARF_REGISTERS.len()),
            repeats: None,
            chain: Vec::new(),
            frames: Vec::new(),
            switched_out: BTreeMap::new(),
            gaps: Vec::new(),
            gaps_end: 0,
            began: thread_cpu(),
            loading: std::time::Duration::ZERO,
            processed: 0,
            window: None,
            segment: None,
            tasks_of: None,
            located: std::collections::VecDeque::new(),
            unclaimed: Vec::new(),
            probes_mapped: BTreeSet::new(),
            probes_found: Vec::new(),
            lost_mappings: std::collections::VecDeque::new(),
            probes_executed: BTreeSet::new(),
            stacks: Stacks::default(),
            triggered: false,
            symbols_later: false,
            interpreted: None,
            paired: None,
        }
    }

    /// Splices the Python frames the BPF engine writes, through the
    /// output events whose identifiers are `outputs`, into the samples.
    pub fn interpret(&mut self, outputs: Vec<u64>) {
        self.interpreted = Some(Interpreted::new(outputs));
    }

    /// What each process's interpreter tells the BPF engine, read now.
    pub fn walked(&mut self) -> Vec<Walked> {
        let Some(interpreted) = &mut self.interpreted else {
            return Vec::new();
        };

        self.processes
            .iter()
            .filter_map(|(&pid, process)| {
                find_interpreter(interpreted, pid, process, &self.images);
                interpreted.walked(pid)
            })
            .collect()
    }

    pub const fn builder(&mut self) -> &mut CaptureBuilder {
        &mut self.builder
    }

    /// How the program at `path` keeps the frame-pointer chain, or `None`
    /// if it cannot be read. Its image is loaded as its mapping will find
    /// it, and loaded once.
    pub fn call_sites(&mut self, path: &std::path::Path) -> Option<Sites> {
        use std::os::unix::fs::MetadataExt;
        let data = std::fs::read(path).ok()?;
        let file = object::File::parse(data.as_slice()).ok()?;
        if object::Object::section_by_name(&file, ".interp").is_some() {
            return Some(Sites::Dynamic);
        }

        let inode = std::fs::metadata(path).ok()?.ino();
        let image = self.image(path.to_owned(), path, Some(inode));
        let info = self.images[image].info.as_ref()?;
        let sites = call_sites(&data, &info.image, info.unwind.as_ref());
        let function = sites.first_not_kept.and_then(|address| {
            info.image
                .symbols()
                .take_while(|symbol| symbol.address().get() <= address)
                .last()
                .map(|symbol| {
                    crate::demangle::qualified_name(symbol.name())
                        .unwrap_or_else(|| symbol.name().to_owned())
                })
        });

        Some(Sites::Static(sites, function))
    }

    /// Drops the samples at the instruction at `address` that `branch`
    /// takes, counting them as repeats.
    pub const fn repeats_when(&mut self, address: u64, branch: Branch) {
        self.repeats = Some((address, branch));
    }

    /// Records what a process already had when an attach began, as its
    /// records describe only what comes after: its name, its threads and
    /// theirs, and its executable mappings.
    pub fn attached(&mut self, pid: Pid, time: u64) {
        let at = time.cast_signed();
        self.first.get_or_insert(time);
        self.latest = self.latest.max(time);
        let name = |path: String| {
            std::fs::read_to_string(path)
                .ok()
                .map(|name| name.trim_end().to_owned())
        };

        let process = self.builder.process(pid, at);
        if let Some(name) = name(format!("/proc/{pid}/comm")) {
            self.builder.process_name(process, &name);
        }

        if let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) {
            let mut tids: Vec<Tid> = tasks
                .filter_map(|task| task.ok()?.file_name().to_str()?.parse().ok())
                .map(Tid::new)
                .collect();
            tids.sort_unstable();

            for tid in tids {
                let thread = self.thread(pid, tid, at);
                if let Some(name) = name(format!("/proc/{pid}/task/{tid}/comm")) {
                    self.builder.thread_name(thread, &name);
                }
            }
        }

        for mapped in perf::mappings(pid.get()).unwrap_or_default() {
            self.mmap(&decode::Mmap {
                pid,
                start: mapped.start,
                length: mapped.end - mapped.start,
                file_offset: mapped.file_offset,
                inode: Some(mapped.inode),
                executable: mapped.executable,
                path: &mapped.path,
                time,
            });
        }
    }

    /// Takes the events' counts so far: what they counted, how many
    /// samples they lost, and how many times a ring's head could not be
    /// trusted, whose skipped spans count as records that could not be
    /// read.
    pub fn counts(&mut self, (counted, lost, untrusted): (u64, u64, u64)) {
        for (kind, total) in [
            (AccountKind::Counted, counted),
            (AccountKind::LostSamples, lost),
        ] {
            let had = self.builder.account(kind as u32);
            self.builder.count(kind as u32, total.saturating_sub(had));
        }

        let skipped = untrusted.saturating_sub(self.untrusted);
        if skipped > 0 {
            self.builder.count(AccountKind::Malformed as u32, skipped);
        }

        self.untrusted = untrusted;
    }

    /// Processes the records in `batch` and those held back before, in
    /// time order, up to `watermark`; holds back the rest. Rings are
    /// drained one after another, so a record of one may be written after
    /// a later record of another was drained: the watermark lies far
    /// enough behind the drain that every record before it has arrived,
    /// and one that still comes later is counted.
    pub fn ingest(&mut self, batch: &[u8], watermark: Option<u64>) {
        let mut bytes = std::mem::take(&mut self.carry);
        bytes.extend_from_slice(batch);
        self.order.clear();
        let outputs = self
            .interpreted
            .as_ref()
            .map(|interpreted| interpreted.outputs.clone())
            .unwrap_or_default();

        // A record of Python frames goes with the next sample of its
        // thread and CPU in its ring (see `interpreted`).
        let mut unpaired: Vec<((Tid, u32), u64, usize, usize)> = Vec::new();
        let mut pairs: HashMap<usize, (interpreted::RecordKey, usize, usize)> = HashMap::default();
        let mut offset = 0;

        while let Some(next) = decode::next_of(&bytes[offset..], &outputs) {
            let Ok((record, length)) = next else {
                self.builder.count(AccountKind::Malformed as u32, 1);
                break;
            };

            let time = record.time().unwrap_or(self.latest);
            match record {
                decode::Record::Output {
                    pid,
                    tid,
                    cpu,
                    data,
                    ..
                } => {
                    let (processes, images) = (&self.processes, &self.images);
                    let interpreted = self.interpreted.as_mut();
                    if let Some(interpreted) = interpreted {
                        if let Some(index) = unpaired.iter().position(|(of, ..)| *of == (tid, cpu))
                        {
                            let (_, superseded, ..) = unpaired.remove(index);
                            interpreted.dropped((tid, cpu, superseded));
                        }

                        let code = |address| code_at(processes, images, pid, address);
                        interpreted.drained(pid, (tid, cpu, time), data, &code);
                    }

                    unpaired.push(((tid, cpu), time, offset, length));
                    offset += length;
                    continue;
                }
                decode::Record::Sample(sample) => {
                    let key = (sample.tid, sample.cpu);
                    if let Some(index) = unpaired.iter().position(|(of, ..)| *of == key) {
                        let (_, time, at, length) = unpaired.swap_remove(index);
                        pairs.insert(offset, ((key.0, key.1, time), at, length));
                    }
                }
                _ => {}
            }

            self.order.push((time, offset, length));
            offset += length;
        }

        self.order.sort_by_key(|(time, offset, _)| (*time, *offset));
        let order = std::mem::take(&mut self.order);

        for &(time, offset, length) in &order {
            let record = &bytes[offset..offset + length];
            let pair = pairs
                .get(&offset)
                .map(|&(key, at, length)| (key, &bytes[at..at + length]));
            if watermark.is_some_and(|watermark| time > watermark) {
                // A held-back sample keeps its frames before it.
                self.carry
                    .extend_from_slice(pair.map(|(_, pair)| pair).unwrap_or_default());
                self.carry.extend_from_slice(record);
                continue;
            }

            self.paired = pair.and_then(|(key, pair)| match decode::next_of(pair, &outputs) {
                Some(Ok((decode::Record::Output { data, .. }, _))) => Some((key, data.to_vec())),
                _ => None,
            });

            if let Some(Ok((record, _))) = decode::next_of(record, &outputs) {
                self.process(record);
            }
        }

        // A record whose sample may still come waits for it; one whose
        // sample the ring dropped goes with it.
        for ((tid, cpu), time, at, length) in unpaired {
            if watermark.is_some_and(|watermark| {
                time.saturating_add(interpreted::PAIRING_SLACK) > watermark
            }) {
                self.carry.extend_from_slice(&bytes[at..at + length]);
            } else if let Some(interpreted) = &mut self.interpreted {
                interpreted.dropped((tid, cpu, time));
            }
        }

        self.paired = None;
        self.order = order;
        self.processed = watermark.unwrap_or(self.latest).max(self.processed);
        self.map_lost_before(watermark.unwrap_or(u64::MAX));
    }

    /// Keeps only a rolling window of `length` of what happens, from
    /// marks taken as [`Processor::mark`] is called.
    pub fn keep_window(&mut self, length: std::time::Duration) {
        self.window = Some(Window {
            length: u64::try_from(length.as_nanos()).unwrap_or(u64::MAX),
            marks: std::collections::VecDeque::new(),
        });
    }

    /// Marks the counts where every record has been processed, and moves
    /// a window's start up to the latest mark it has passed, forgetting
    /// what came before. The events' counts must be current.
    pub fn mark(&mut self) {
        let Some(window) = &mut self.window else {
            return;
        };

        let now = self.processed;
        window
            .marks
            .push_back((now, self.builder.accounts().clone()));
        let passed = now.saturating_sub(window.length);
        while window.marks.get(1).is_some_and(|(time, _)| *time <= passed) {
            window.marks.pop_front();
        }

        if let Some(&(start, _)) = window.marks.front().filter(|(time, _)| *time <= passed) {
            self.builder.forget_before(start.cast_signed());
            self.first = Some(start);
        }
    }

    /// Where a window starts, and the counts there.
    fn window_start(&self) -> Option<&(u64, BTreeMap<u32, u64>)> {
        let window = self.window.as_ref()?;
        let passed = self.processed.saturating_sub(window.length);
        window.marks.front().filter(|(time, _)| *time <= passed)
    }

    fn process(&mut self, record: decode::Record<'_>) {
        let time = record.time().unwrap_or(self.latest);
        self.map_lost_before(time);
        if time < self.latest {
            self.builder.count(AccountKind::LateRecords as u32, 1);
        }

        self.latest = self.latest.max(time);
        self.first.get_or_insert(time);
        let at = time.cast_signed();

        match record {
            decode::Record::Sample(sample) => self.sample(&sample),
            decode::Record::Mmap(mmap) => self.mmap(&mmap),
            decode::Record::Comm {
                pid,
                tid,
                name,
                exec,
                ..
            } => {
                let name = String::from_utf8_lossy(name);
                let thread = self.thread(pid, tid, at);
                self.builder.thread_name(thread, &name);
                if exec {
                    // A new program replaces the address space, and the
                    // interpreter and compiled code it held.
                    let state = self.processes.entry(pid).or_default();
                    state.mapped.clear();
                    state.before_v8.clear();
                    state.jit.exec();
                    if let Some(interpreted) = &mut self.interpreted {
                        interpreted.forget(pid);
                    }

                    // Its instrumentation went with it; the new program's
                    // is found as it maps.
                    self.probes_mapped.retain(|(process, _)| *process != pid);
                    self.probes_found.retain(|(process, _)| *process != pid);
                    self.probes_executed.insert(pid);
                    let process = self.builder.process(pid, at);
                    self.builder.process_name(process, &name);
                }
            }
            decode::Record::Fork { pid, ppid, tid, .. } => {
                if tid == pid.leader() && pid != ppid {
                    // A forked process starts with its parent's mappings.
                    let (mapped, before_v8) = self
                        .processes
                        .get(&ppid)
                        .map(|parent| (parent.mapped.clone(), parent.before_v8.clone()))
                        .unwrap_or_default();
                    let child = self.processes.entry(pid).or_default();
                    child.mapped = mapped;
                    child.before_v8 = before_v8;
                }

                self.thread(pid, tid, at);
            }

            decode::Record::Switch {
                pid,
                tid,
                out,
                preempted,
                ..
            } => {
                let thread = self.thread(pid, tid, at);

                if out {
                    self.switched_out.insert(tid, (thread, time, preempted));
                } else if let Some((thread, left, preempted)) = self.switched_out.remove(&tid) {
                    let back = time.min(self.next_gap(left));
                    self.builder.off_cpu(
                        thread,
                        left.cast_signed(),
                        back.saturating_sub(left),
                        preempted,
                    );
                }

                // A thread that comes back unseen leaving left before the
                // recording could see it, so when is unknown.
            }
            decode::Record::Exit { pid, tid, .. } => {
                self.switched_out.remove(&tid);
                self.builder.thread_exit(tid, at);
                if let Some(interpreted) = &mut self.interpreted {
                    interpreted.thread_exited(pid, tid);
                }

                let ended = self.processes.get_mut(&pid).is_some_and(|process| {
                    process.threads.remove(&tid);
                    process.threads.is_empty()
                });
                if ended {
                    self.processes.remove(&pid);
                    self.builder.process_exit(pid, at);
                }
            }

            // A loss record says only what its ring lost since it last had
            // room, and a ring that never has room again never says: the
            // events' own counts are exact, and `lost` takes them.
            decode::Record::Throttle { .. } => {
                self.builder.count(AccountKind::Throttles as u32, 1);
            }
            decode::Record::Lost { time, .. } => self.lost(time),
            // An output's record is processed with its sample.
            decode::Record::Output { .. } | decode::Record::Other { .. } => {}
        }
    }

    fn thread(&mut self, pid: Pid, tid: Tid, time: i64) -> u32 {
        self.processes.entry(pid).or_default().threads.insert(tid);
        self.builder.thread(pid, tid, time)
    }

    fn mmap(&mut self, mmap: &decode::Mmap<'_>) {
        let end = mmap.start.saturating_add(mmap.length);
        let process = self.builder.process(mmap.pid, mmap.time.cast_signed());
        let image = if perf::is_vdso(mmap) {
            self.vdso().map(|(image, link)| {
                let start = crate::VirtualAddress::new(mmap.start);
                (image, LoadBias::between(ImageAddress::new(link), start))
            })
        } else {
            perf::identify(mmap).map(|(file, bias)| {
                let path = PathBuf::from(String::from_utf8_lossy(mmap.path).into_owned());
                (self.image(path, &file, mmap.inode), bias)
            })
        };

        let state = self.processes.entry(mmap.pid).or_default();

        // A mapping replaces whatever it overlaps.
        uncover(&mut state.mapped, mmap.start, end);
        state.before_v8 = std::mem::take(&mut state.before_v8)
            .into_iter()
            .flat_map(|code| {
                [
                    code.start..code.end.min(mmap.start),
                    code.start.max(end)..code.end,
                ]
            })
            .filter(|piece| !piece.is_empty())
            .collect();
        state.mappings += 1;
        state.jit.mapped(
            mmap.start,
            end,
            perf::jit::is_anonymous_code(mmap),
            mmap.time,
        );

        let Some((image, bias)) = image else {
            if perf::jit::is_anonymous_code(mmap) {
                let carries_v8 = state.mapped.iter().any(|mapped| {
                    self.images[mapped.image]
                        .info
                        .as_ref()
                        .is_some_and(|info| crate::debug_info::v8::is_v8(&info.image))
                });
                if carries_v8 {
                    self.v8_code(mmap.pid, mmap.time, mmap.start..end);
                } else {
                    state.before_v8.push(mmap.start..end);
                }
            }
            return;
        };

        if let Some(table) = &self.images[image].probes
            && self.probes_mapped.insert((mmap.pid, image))
        {
            let found = Found {
                module: self.images[image].module,
                path: String::from_utf8_lossy(mmap.path).into(),
                bias,
                table: Arc::clone(table),
            };

            self.probes_found.push((mmap.pid, found));
        }

        let loaded = LoadedModule {
            id: ModuleId::new(self.next_module),
            image: self.images[image].id,
            load_bias: bias,
            window: None,
        };

        self.next_module += 1;

        state.mapped.push(Mapped {
            start: mmap.start,
            end,
            image,
            loaded,
        });
        self.builder.mapping(Mapping {
            process: process.into(),
            module: self.images[image].module.into(),
            start: mmap.start.into(),
            end: end.into(),
            file_offset: mmap.file_offset.into(),
            bias: bias.get().into(),
            time: mmap.time.cast_signed().into(),
        });

        let state = self.processes.entry(mmap.pid).or_default();
        let carries_v8 = !state.before_v8.is_empty()
            && self.images[image]
                .info
                .as_ref()
                .is_some_and(|info| crate::debug_info::v8::is_v8(&info.image));
        if carries_v8 {
            for code in std::mem::take(&mut state.before_v8) {
                self.v8_code(mmap.pid, mmap.time, code);
            }
        }
    }

    /// The vDSO's image, which no file holds, and the address it links at.
    fn vdso(&mut self) -> Option<(usize, u64)> {
        if self.vdso_read {
            return self.vdso;
        }

        self.vdso_read = true;
        let vdso = perf::own_vdso().map(|(data, link)| {
            (
                self.add_image(std::path::Path::new("[vdso]"), Some(data)),
                link,
            )
        });
        self.vdso = vdso;
        vdso
    }

    /// The image of the file at `path`, loading it the first time.
    fn image(&mut self, path: PathBuf, file: &std::path::Path, inode: Option<u64>) -> usize {
        let key = (path, inode.unwrap_or(0));
        if let Some(&image) = self.image_index.get(&key) {
            return image;
        }

        let data = crate::image::backing::read_input(file)
            .ok()
            .map(|(data, _)| data);
        let image = self.add_image(&key.0, data);
        self.image_index.insert(key, image);
        image
    }

    /// Adds the image of a module whose file holds `data`, if it could be
    /// read.
    fn add_image(&mut self, path: &std::path::Path, data: Option<Vec<u8>>) -> usize {
        let began = thread_cpu();
        let image = self.load_image(path, data);
        self.loading += thread_cpu().saturating_sub(began);
        image
    }

    fn load_image(&mut self, path: &std::path::Path, data: Option<Vec<u8>>) -> usize {
        let id = ModuleImageId::new(u32::try_from(self.images.len()).unwrap_or(u32::MAX));
        let parsed = data
            .as_ref()
            .and_then(|data| object::File::parse(data.as_slice()).ok());
        let build_id = parsed
            .as_ref()
            .and_then(|file| object::Object::build_id(file).ok().flatten().map(hex));
        let probes = parsed.as_ref().map(probes::table);
        drop(parsed);
        let digest = data.as_deref().map_or([0; 16], |data| {
            twox_hash::XxHash3_128::oneshot(data).to_le_bytes()
        });
        let size = data.as_ref().map_or(0, |data| data.len() as u64);
        let module =
            self.builder
                .module(&path.to_string_lossy(), build_id.as_deref(), digest, size);
        let info = data.and_then(|data| {
            crate::debug_info::load_module_bytes(path, &data, id, &self.search).ok()
        });

        let probes = match probes {
            Some(Ok(table)) => table.map(Arc::new),
            Some(Err(why)) => {
                let text = format!(
                    "{}: its instrumentation cannot be read: {why}",
                    path.display()
                );
                self.builder
                    .marker(self.latest.cast_signed(), MarkerKind::Probes, &text);
                None
            }
            None => None,
        };

        let image = self.images.len();
        self.images.push(Image {
            info,
            id,
            module,
            probes,
            anonymous: false,
            generated: false,
        });
        image
    }

    /// Makes the module of anonymous code a perf map names, mapped at the
    /// addresses it names, in place of what the range held. In a process
    /// that carries V8, as Node's map under `--perf-basic-prof` names its
    /// code, the module of each piece V8 generated is made again with the
    /// map's functions for its symbols, and still V8's convention for its
    /// frames: the map's own proof of a frame does not follow V8's, and the
    /// module gives V8's frames their JavaScript functions. A copy of a
    /// module's code keeps that module, which names it already.
    fn perf_map_module(&mut self, pid: Pid, time: u64, stale: &Stale) {
        let held: &[Mapped] = self
            .processes
            .get(&pid)
            .map_or(&[], |process| &process.mapped);
        let carries_v8 = held.iter().any(|mapped| {
            self.images[mapped.image]
                .info
                .as_ref()
                .is_some_and(|info| crate::debug_info::v8::is_v8(&info.image))
        });
        if carries_v8 {
            let generated: Vec<std::ops::Range<u64>> = held
                .iter()
                .filter(|mapped| {
                    self.images[mapped.image].generated
                        && mapped.start < stale.end
                        && stale.start < mapped.end
                })
                .map(|mapped| mapped.start.max(stale.start)..mapped.end.min(stale.end))
                .collect();
            for code in generated {
                self.generated_module(pid, time, code, &stale.functions);
                // A module is found by its image's bounds, and those of the
                // module it replaces may span more than its pieces.
                let held = &mut self.processes.entry(pid).or_default().mapped;
                if let Some(named) = held.pop() {
                    held.insert(0, named);
                }
            }
            return;
        }

        let began = thread_cpu();
        let id = ModuleImageId::new(u32::try_from(self.images.len()).unwrap_or(u32::MAX));
        let range = crate::AddressRange {
            start: ImageAddress::new(stale.start),
            end: ImageAddress::new(stale.end),
        };
        let mut digest = twox_hash::XxHash3_128::new();
        digest.write(&stale.start.to_le_bytes());

        for function in &stale.functions {
            digest.write(&function.entry.start.to_le_bytes());
            digest.write(&function.entry.end.to_le_bytes());
            digest.write(function.entry.name.as_bytes());
        }

        let info = crate::debug_info::perf_map::load(
            std::path::Path::new(&stale.path),
            id,
            range,
            &stale.functions,
        )
        .ok();
        let code = stale.start..stale.end;
        self.anonymous_module(
            pid,
            time,
            (id, &stale.path, digest.finish_128()),
            code,
            info,
        );
        self.loading += thread_cpu().saturating_sub(began);
    }

    /// Makes the module of an object a JIT compiler registered, mapped at
    /// the addresses its code runs at, in place of what they held.
    fn jit_object_module(&mut self, pid: Pid, time: u64, object: &JitCode) {
        let began = thread_cpu();
        let id = ModuleImageId::new(u32::try_from(self.images.len()).unwrap_or(u32::MAX));
        let info = crate::debug_info::load_transient_bytes(
            std::path::Path::new(&object.path),
            &object.bytes,
            id,
        )
        .ok();
        let digest = twox_hash::XxHash3_128::oneshot(&object.bytes);
        let code = object.start..object.end;
        self.anonymous_module(pid, time, (id, &object.path, digest), code, info);
        self.loading += thread_cpu().saturating_sub(began);
    }

    /// Maps a module over anonymous `code` of process `pid`, where a module
    /// it maps carries V8, as the syscall tracer does: a copy of a module's
    /// code, proven byte for byte while the process runs, is that module's
    /// code at another address, and any other is code V8 generated, whose
    /// frames keep V8's frame-pointer convention
    /// (`debug_info::runtime_code`). Code the process may write is never
    /// taken for a copy, which it may since have changed.
    fn v8_code(&mut self, pid: Pid, time: u64, code: std::ops::Range<u64>) {
        let Some(process) = self.processes.get(&pid) else {
            return;
        };
        let v8: Vec<(usize, &Mapped, &DebugInfo)> = process
            .mapped
            .iter()
            .enumerate()
            .filter_map(|(index, mapped)| {
                let info = self.images[mapped.image].info.as_ref()?;
                crate::debug_info::v8::is_v8(&info.image).then_some((index, mapped, info))
            })
            .collect();
        if v8.is_empty() {
            return;
        }

        let read_only = perf::mappings(pid.get()).is_ok_and(|mappings| {
            mappings.iter().any(|mapping| {
                mapping.start <= code.start && code.end <= mapping.end && !mapping.writable
            })
        });
        let copy = read_only
            .then(|| {
                let sources: Vec<crate::unwind::code_copies::Source<'_, usize>> = v8
                    .iter()
                    .map(
                        |&(index, mapped, info)| crate::unwind::code_copies::Source {
                            key: index,
                            image: &info.image,
                            loaded: mapped.loaded,
                            stored: perf::stored_code(
                                pid.get(),
                                info,
                                mapped.loaded,
                                mapped.start..mapped.end,
                            ),
                        },
                    )
                    .collect();
                perf::copy_of(pid.get(), &code, &sources)
            })
            .flatten()
            .and_then(|proven| {
                let source = process.mapped.get(proven.source)?;
                Some((*source, proven.image_start, proven.length))
            });
        drop(v8);

        let Some((source, image_start, length)) = copy else {
            self.generated_module(pid, time, code, &[]);
            return;
        };

        let end = code.start.saturating_add(length);
        let bias = LoadBias::between(image_start, crate::VirtualAddress::new(code.start));
        let loaded = LoadedModule {
            id: ModuleId::new(self.next_module),
            image: source.loaded.image,
            load_bias: bias,
            window: Some(crate::AddressRange {
                start: image_start,
                end: ImageAddress::new(image_start.get().saturating_add(length)),
            }),
        };
        self.next_module += 1;
        let module = self.images[source.image].module;
        let process = self.builder.process(pid, time.cast_signed());
        let held = &mut self.processes.entry(pid).or_default().mapped;
        uncover(held, code.start, end);
        // A module is found by its image's bounds, and those of the code V8
        // generated span its whole code range, which holds the copy: the
        // copy comes first.
        held.insert(
            0,
            Mapped {
                start: code.start,
                end,
                image: source.image,
                loaded,
            },
        );
        self.builder.mapping(Mapping {
            process: process.into(),
            module: module.into(),
            start: code.start.into(),
            end: end.into(),
            file_offset: 0.into(),
            bias: bias.get().into(),
            time: time.cast_signed().into(),
        });
    }

    /// Maps the module of `code` that V8 generated, whose frames keep V8's
    /// frame-pointer convention, with the functions of `jitted` a perf map
    /// names there for its symbols.
    fn generated_module(
        &mut self,
        pid: Pid,
        time: u64,
        code: std::ops::Range<u64>,
        jitted: &[crate::debug_info::perf_map::Jitted],
    ) {
        let began = thread_cpu();
        let id = ModuleImageId::new(u32::try_from(self.images.len()).unwrap_or(u32::MAX));
        let path = format!("[V8 code {:#x}]", code.start);
        let bounds = crate::AddressRange {
            start: ImageAddress::new(code.start),
            end: ImageAddress::new(code.end),
        };
        let info = crate::debug_info::perf_map::load_runtime_code(
            std::path::Path::new(&path),
            id,
            bounds,
            jitted,
        )
        .ok();
        let mut digest = twox_hash::XxHash3_128::new();
        digest.write(path.as_bytes());

        for function in jitted {
            digest.write(&function.entry.start.to_le_bytes());
            digest.write(&function.entry.end.to_le_bytes());
            digest.write(function.entry.name.as_bytes());
        }

        self.anonymous_module(pid, time, (id, &path, digest.finish_128()), code, info);
        if let Some(image) = self.images.last_mut() {
            image.generated = true;
        }
        self.loading += thread_cpu().saturating_sub(began);
    }

    /// Maps the module of anonymous code `id` named `path`, which `digest`
    /// identifies, over `code`, replacing whatever the process mapped there.
    fn anonymous_module(
        &mut self,
        pid: Pid,
        time: u64,
        (id, path, digest): (ModuleImageId, &str, u128),
        code: std::ops::Range<u64>,
        info: Option<DebugInfo>,
    ) {
        let module = self
            .builder
            .module(path, None, digest.to_le_bytes(), code.end - code.start);
        let image = self.images.len();
        self.images.push(Image {
            info,
            id,
            module,
            probes: None,
            anonymous: true,
            generated: false,
        });

        let loaded = LoadedModule {
            id: ModuleId::new(self.next_module),
            image: id,
            load_bias: LoadBias::new(0),
            window: None,
        };
        self.next_module += 1;
        let process = self.builder.process(pid, time.cast_signed());
        let held = &mut self.processes.entry(pid).or_default().mapped;
        uncover(held, code.start, code.end);
        held.push(Mapped {
            start: code.start,
            end: code.end,
            image,
            loaded,
        });
        self.builder.mapping(Mapping {
            process: process.into(),
            module: module.into(),
            start: code.start.into(),
            end: code.end.into(),
            file_offset: 0.into(),
            bias: 0.into(),
            time: time.cast_signed().into(),
        });
    }

    /// Unwinds a sample through the modules of its process, with the
    /// mappings its frames' modules index.
    fn unwind(
        &mut self,
        sample: &decode::Sample<'_>,
    ) -> (
        Vec<crate::unwind::sample::SampledFrame>,
        StackEnd,
        Vec<Mapped>,
    ) {
        let mapped: &[Mapped] = self
            .processes
            .get(&sample.pid)
            .map_or(&[], |process| &process.mapped);
        let mut owners = Vec::with_capacity(mapped.len());
        let modules: Vec<UnwindModule<'_>> = mapped
            .iter()
            .filter_map(|mapped| {
                let info = self.images[mapped.image].info.as_ref()?;
                owners.push(*mapped);
                Some(UnwindModule {
                    loaded: mapped.loaded,
                    image: &info.image,
                    unwind: info.unwind.as_ref(),
                })
            })
            .collect();

        let (frames, end) = unwind_sample(
            &modules,
            &mut self.rules,
            SampledState {
                registers: &self.registers,
                stack: sample.stack,
                chain: &self.chain,
            },
            self.limit,
        );
        (frames, end, owners)
    }

    /// Makes again the modules of anonymous code a sample's frames or
    /// chain landed in that the perf map of its process may name since:
    /// whether any changed.
    fn refresh_perf_map(
        &mut self,
        sample: &decode::Sample<'_>,
        frames: &[crate::unwind::sample::SampledFrame],
    ) -> bool {
        let addresses: Vec<u64> = frames
            .iter()
            .map(|frame| frame.address.get())
            .chain(self.chain.first().copied())
            .chain(
                self.chain
                    .iter()
                    .skip(1)
                    .map(|address| address.wrapping_sub(1)),
            )
            .collect();
        let recovered = self.map_unseen_code(sample, &addresses);

        let Some(process) = self.processes.get_mut(&sample.pid) else {
            return recovered;
        };

        let images = &self.images;
        let mapped = &process.mapped;
        let descriptors = || {
            let descriptors: BTreeSet<u64> = mapped
                .iter()
                .filter_map(|mapped| Some((mapped, images[mapped.image].info.as_ref()?)))
                .flat_map(|(mapped, info)| {
                    info.image
                        .symbols_named(gdb_jit::DESCRIPTOR)
                        .filter_map(|symbol| mapped.loaded.virtual_address(symbol.address()).ok())
                        .map(crate::VirtualAddress::get)
                        .collect::<Vec<_>>()
                })
                .collect();
            descriptors.into_iter().collect()
        };

        // Code V8 generated is anonymous code too, which the map may name.
        let unnamed = addresses.iter().copied().filter(|&address| {
            mapped
                .iter()
                .find(|mapped| (mapped.start..mapped.end).contains(&address))
                .is_none_or(|mapped| {
                    let image = &images[mapped.image];
                    image.generated
                        && !image.info.as_ref().is_some_and(|info| {
                            info.image.symbolize(ImageAddress::new(address)).is_some()
                        })
                })
        });
        let refreshed = process
            .jit
            .refresh(sample.pid.get(), sample.time, unnamed, descriptors);
        if let Some(problem) = process.jit.take_problem() {
            self.builder.setting(
                &format!("perf map {}", sample.pid),
                &format!("not read: {problem}"),
            );
        }

        for stale in &refreshed.stale {
            self.perf_map_module(sample.pid, sample.time, stale);
        }

        for object in &refreshed.objects {
            self.jit_object_module(sample.pid, sample.time, object);
        }

        recovered || !refreshed.stale.is_empty() || !refreshed.objects.is_empty()
    }

    /// Reads each process's mappings after a ring lost records at `time`,
    /// as the mapping of a module may have been among them: a module
    /// whose mapping was never seen has no frames, and its instrumentation
    /// is never found. A read after the loss covers it already.
    ///
    /// What a process maps now may be newer than the loss, its record not
    /// processed yet, so the mappings read wait until every record before
    /// they were read has been (see [`Processor::map_lost_before`]).
    fn lost(&mut self, time: u64) {
        if self
            .lost_mappings
            .back()
            .is_some_and(|(read, _)| time < *read)
        {
            return;
        }

        let read = super::session::monotonic();
        let mappings = self
            .processes
            .keys()
            .filter_map(|&pid| Some((pid, perf::mappings(pid.get()).ok()?)))
            .collect();
        record!("record: a ring lost records at {time}, mappings read at {read}");
        self.lost_mappings.push_back((read, mappings));
    }

    /// Maps the modules read after a loss before `time` that no record
    /// has mapped where they are, as their records would have. Every
    /// record before the read has been processed by then, so what is
    /// still missing was lost. Anonymous code is left to the samples that
    /// run it (see [`Processor::map_unseen_code`]).
    fn map_lost_before(&mut self, time: u64) {
        while let Some((read, _)) = self.lost_mappings.front()
            && *read <= time
        {
            let Some((read, processes)) = self.lost_mappings.pop_front() else {
                break;
            };

            for (pid, mappings) in processes {
                for mapping in &mappings {
                    let mmap = decode::Mmap {
                        pid,
                        start: mapping.start,
                        length: mapping.end - mapping.start,
                        file_offset: mapping.file_offset,
                        inode: Some(mapping.inode),
                        executable: mapping.executable,
                        path: &mapping.path,
                        time: read,
                    };

                    if !self.holds(&mmap) && perf::identify(&mmap).is_some() {
                        record!(
                            "record: mapped {} at {:#x} after a loss",
                            String::from_utf8_lossy(mmap.path),
                            mmap.start
                        );
                        self.mmap(&mmap);
                    }
                }
            }
        }
    }

    /// Whether the process maps `mmap`'s file over all of it already.
    fn holds(&self, mmap: &decode::Mmap<'_>) -> bool {
        let key = (
            PathBuf::from(String::from_utf8_lossy(mmap.path).into_owned()),
            mmap.inode.unwrap_or(0),
        );
        let end = mmap.start.saturating_add(mmap.length);
        let (Some(&image), Some(process)) =
            (self.image_index.get(&key), self.processes.get(&mmap.pid))
        else {
            return false;
        };

        process
            .mapped
            .iter()
            .any(|mapped| mapped.image == image && mapped.start <= mmap.start && end <= mapped.end)
    }

    /// Maps each anonymous executable mapping the process holds now that
    /// holds one of a sample's `addresses` the recording has seen nothing
    /// of, as its record would have: a full ring loses mapping records with
    /// everything else, and mremap moves code where no record says. Its
    /// perf map is then read for it. Whether any was mapped.
    ///
    /// Only anonymous code is mapped so: a perf map names it by address,
    /// as it is read later anyway, but a file the process maps now where
    /// none was seen may have replaced what the sample ran.
    fn map_unseen_code(&mut self, sample: &decode::Sample<'_>, addresses: &[u64]) -> bool {
        let Some(process) = self.processes.get(&sample.pid) else {
            return false;
        };

        let unseen: Vec<u64> = addresses
            .iter()
            .copied()
            .filter(|&address| {
                !process
                    .mapped
                    .iter()
                    .any(|mapped| (mapped.start..mapped.end).contains(&address))
                    && process.jit.unseen(address, sample.time)
            })
            .collect();
        if unseen.is_empty() {
            return false;
        }

        let Ok(mappings) = perf::mappings(sample.pid.get()) else {
            return false;
        };

        let mut mapped = false;
        for mapping in &mappings {
            let mmap = decode::Mmap {
                pid: sample.pid,
                start: mapping.start,
                length: mapping.end - mapping.start,
                file_offset: mapping.file_offset,
                inode: Some(mapping.inode),
                executable: mapping.executable,
                path: &mapping.path,
                time: sample.time,
            };
            let holds = unseen
                .iter()
                .any(|address| (mapping.start..mapping.end).contains(address));

            if holds && perf::jit::is_anonymous_code(&mmap) {
                self.mmap(&mmap);
                mapped = true;
            }
        }

        mapped
    }

    fn sample(&mut self, sample: &decode::Sample<'_>) {
        let time = sample.time.cast_signed();
        let thread = self.thread(sample.pid, sample.tid, time);
        self.registers.clear();
        self.registers.extend(
            decode::DWARF_REGISTERS
                .iter()
                .copied()
                .zip(sample.registers.iter()),
        );

        if let Some((address, branch)) = self.repeats {
            let register = |number| {
                self.registers
                    .iter()
                    .find_map(|(register, value)| (*register == number).then_some(*value))
            };

            let at = register(crate::unwind::RETURN_ADDRESS) == Some(address);
            if at && register(decode::DWARF_FLAGS).is_some_and(|flags| branch.taken(flags)) {
                self.builder.count(AccountKind::Repeats as u32, 1);
                return;
            }
        }

        self.chain.clear();
        self.chain.extend(sample.user_chain());
        self.builder
            .count(AccountKind::StackBytes as u32, sample.stack.len() as u64);

        let (mut frames, mut end, mut owners) = self.unwind(sample);
        if self.refresh_perf_map(sample, &frames) {
            (frames, end, owners) = self.unwind(sample);
        }

        if let Some(leaf) = frames.first() {
            self.note_task(sample.pid, thread, time, leaf, &owners);
        }

        let (native, keys) = self.native_frames(&frames, &owners);
        let (interpreted, running) = self.sampled_interpreted(sample, thread);
        let native: Vec<(u32, bool)> = (0..native.len())
            .map(|index| {
                let inner = index
                    .checked_sub(1)
                    .and_then(|inner| native[inner].1.as_deref());
                let activation = native[index].1.as_deref().is_some_and(|name| {
                    self.interpreted
                        .as_ref()
                        .is_some_and(|found| found.activation(sample.pid, name, inner))
                });
                (native[index].0, activation)
            })
            .collect();

        // The asyncio task the thread ran is claimed as a task kept in a
        // register is.
        if let Some(task) = running
            && self.tasks_of == Some(sample.pid)
        {
            self.unclaimed.push((thread, time, task));
        }

        // JavaScript frames that are unknown, or that a walk could not
        // follow to the end, are missing where V8's code runs.
        let missing = matches!(
            interpreted,
            Sampled::Unknown
                | Sampled::Placed {
                    unreadable: true,
                    ..
                }
        );
        let runs_v8 = missing
            && self.interpreted.as_ref().is_some_and(|found| {
                let (processes, images) = (&self.processes, &self.images);
                let code = |address| code_at(processes, images, sample.pid, address);
                frames
                    .iter()
                    .any(|frame| found.runs_v8(sample.pid, &code, frame.address))
            });
        let cut = interpreted::v8_cut(&interpreted, runs_v8);
        let marker = if let Sampled::Placed { frames: placed, .. } = interpreted {
            self.splice_placed(&native, (&frames, &keys), &placed);
            cut
        } else {
            self.splice_interpreted(&native, interpreted).or(cut)
        };

        let truncation = match end {
            StackEnd::Truncated(reason, module) => Some((
                reason,
                module.map_or(NONE, |module| self.images[owners[module].image].module),
            )),
            StackEnd::Complete => marker.map(|marker| (marker, NONE)),
        };

        if let Some((reason, module)) = truncation {
            let frame = self.builder.truncation(reason, module);
            self.frames.push(frame);
            self.builder
                .count(AccountKind::Truncated as u32 + reason as u32, 1);
        }

        let stack = self.builder.stack(&self.frames);
        self.builder
            .sample(thread, time, sample.period, stack, NONE);
    }

    /// The Python frames of `sample`, of the thread whose row is `thread`,
    /// from the record of them paired with it, and the asyncio task it ran;
    /// and the thread's GIL label at it.
    fn sampled_interpreted(
        &mut self,
        sample: &decode::Sample<'_>,
        thread: u32,
    ) -> (Sampled, Option<u64>) {
        let paired = self.paired.take();
        let Some(interpreted) = &mut self.interpreted else {
            return (Sampled::None, None);
        };

        if let Some(process) = self.processes.get(&sample.pid) {
            find_interpreter(interpreted, sample.pid, process, &self.images);
        }

        let stack = paired
            .as_ref()
            .filter(|(_, bytes)| !crate::backend::bpf::sampler::is_v8(bytes))
            .and_then(|(_, bytes)| crate::backend::bpf::sampler::decode(bytes).ok());
        let time = sample.time.cast_signed();
        if let Some(stack) = &stack {
            interpreted.label(&mut self.builder, thread, (time, sample.period), stack.gil);
        }
        let javascript = paired
            .as_ref()
            .filter(|(_, bytes)| crate::backend::bpf::sampler::is_v8(bytes))
            .and_then(|(_, bytes)| crate::backend::bpf::sampler::decode_v8(bytes).ok());
        if let Some(javascript) = javascript {
            interpreted.isolate(
                &mut self.builder,
                (sample.pid, sample.tid, thread),
                (time, sample.period),
                javascript.isolate,
            );
        }

        let (processes, images) = (&self.processes, &self.images);
        let code = |address| code_at(processes, images, sample.pid, address);
        (
            interpreted.paired(
                sample.pid,
                paired.as_ref().map(|(key, bytes)| (*key, bytes.as_slice())),
                &code,
            ),
            stack.and_then(|stack| stack.running_task),
        )
    }

    /// Each of a sample's native `frames`, of the modules `owners`, as a
    /// frame of the capture with the name of the interpreter's code it
    /// runs, if it runs the interpreter's; and each frame's module and
    /// address, its key.
    fn native_frames(
        &mut self,
        frames: &[crate::unwind::sample::SampledFrame],
        owners: &[Mapped],
    ) -> NativeFrames {
        let mut native = Vec::with_capacity(frames.len());
        let mut keys = Vec::with_capacity(frames.len());
        for frame in frames {
            let owner = frame
                .module
                .map(|module| owners[module])
                .filter(|owner| self.names(owner, frame.address.get()));
            let key = owner.map_or_else(
                || (NONE, frame.address.get()),
                |owner| {
                    (
                        self.images[owner.image].module,
                        owner.loaded.load_bias.wrapping_unload(frame.address).get(),
                    )
                },
            );
            // Code V8 generated runs only its functions and its own
            // machinery, as an interpreter's loop does: a sample's frames
            // of V8's name its functions where it has them.
            let generated = frame
                .module
                .is_some_and(|module| self.images[owners[module].image].generated);
            let role = if generated {
                role_number(CodeRole::Interpreter)
            } else if owner.is_some() {
                role_number(frame.role)
            } else {
                0
            };
            let id = self.builder.frame(FrameKind::Native, key.0, key.1, role);
            keys.push(key);

            // Only the interpreter's own code can be its loop, or call
            // from it.
            let name = owner
                .filter(|_| frame.role == CodeRole::Interpreter)
                .and_then(|owner| {
                    let info = self.images[owner.image].info.as_ref()?;
                    let address = owner.loaded.load_bias.wrapping_unload(frame.address);
                    Some(info.image.symbolize(address)?.name)
                });
            native.push((id, name));
        }

        (native, keys)
    }

    /// Puts the stack's frames in [`Self::frames`]: its `native` frames,
    /// each with whether it is an activation of an interpreter's loop,
    /// and the `interpreted` frames spliced in. Returns where the
    /// interpreted frames say the stack ends early, if they do.
    fn splice_interpreted(
        &mut self,
        native: &[(u32, bool)],
        interpreted: Sampled,
    ) -> Option<Truncation> {
        self.frames.clear();
        let natives = native.iter().map(|&(frame, _)| frame);
        let (frames, ended) = match interpreted {
            // The caller splices placed frames.
            Sampled::None | Sampled::Placed { .. } => {
                self.frames.extend(natives);
                return None;
            }
            Sampled::Unknown => {
                self.frames.extend(natives);
                return interpreted::unknown_cuts(native).then_some(Truncation::InterpretedUnknown);
            }
            Sampled::Frames { frames, ended } => (frames, ended),
        };

        let mut segments = vec![Vec::new()];

        for frame in &frames {
            let wait = InterpretedWait::of_frame(frame);
            let id = self.builder.interpreted(&InterpretedPlace {
                function: &wait.function,
                file: wait.file.as_deref(),
                line: wait.line,
                declared: wait.first_line,
            });
            if let Some(segment) = segments.last_mut() {
                segment.push(id);
            }

            if frame.outermost {
                segments.push(Vec::new());
            }
        }

        // The split leaves an empty segment after the last outermost.
        if segments.last().is_some_and(Vec::is_empty) {
            segments.pop();
        }

        if let Some(spliced) = interpreted::splice(native, &segments) {
            self.frames = spliced;
            return ended.then_some(Truncation::InterpretedEnded);
        }

        self.frames.extend(natives);
        self.frames.extend(segments.into_iter().flatten());
        Some(Truncation::InterpretedUnplaced)
    }

    /// Puts the stack's frames in [`Self::frames`]: its `native` frames,
    /// each `placed` frame's functions just before the native frame of
    /// `frames` it is, by its frame pointer and address, with that frame's
    /// key in `keys` as the interpreter's, or past the last where the native
    /// stack ended before it. One among the native frames that is none of
    /// them is no frame of the runtime's.
    fn splice_placed(
        &mut self,
        native: &[(u32, bool)],
        (frames, keys): (&[crate::unwind::sample::SampledFrame], &[(u32, u64)]),
        placed: &[interpreted::PlacedFrame],
    ) {
        self.frames.clear();
        let natives: Vec<_> = frames
            .iter()
            .map(|frame| (frame.address, frame.frame_pointer))
            .collect();
        let walked: Vec<_> = placed
            .iter()
            .map(|frame| (frame.frame_pointer, frame.address))
            .collect();
        let positions = interpreted::positions(&natives, &walked);
        let mut before: Vec<Vec<u32>> = vec![Vec::new(); native.len() + 1];
        for (frame, index) in placed.iter().zip(positions) {
            for function in &frame.functions {
                let wait = InterpretedWait::of(Some(function));
                let id = self.builder.interpreted(&InterpretedPlace {
                    function: &wait.function,
                    file: wait.file.as_deref(),
                    line: wait.line,
                    declared: wait.first_line,
                });
                if let Some(slot) = before.get_mut(index) {
                    slot.push(id);
                }
            }
        }
        let interpreter = role_number(CodeRole::Interpreter);
        for (index, &(id, _)) in native.iter().enumerate() {
            let functions = before.get(index).map_or(&[][..], Vec::as_slice);
            // A frame that runs the runtime's functions stands for them, as
            // an interpreter's frame does. One of code that also runs apart
            // from them, as a bytecode handler the sample landed in, which
            // the frame of its function runs, is what they run.
            let own = self
                .builder
                .frames()
                .get(id as usize)
                .map(|frame| frame.role);
            if !functions.is_empty() && own != Some(interpreter) {
                self.frames.push(id);
                self.frames.extend_from_slice(functions);
                continue;
            }
            self.frames.extend_from_slice(functions);
            let id = match (functions.is_empty(), keys.get(index)) {
                (false, Some(&(module, address))) => {
                    self.builder
                        .frame(FrameKind::Native, module, address, interpreter)
                }
                _ => id,
            };
            self.frames.push(id);
        }
        if let Some(outer) = before.last() {
            self.frames.extend_from_slice(outer);
        }
    }

    /// The capture so far, every frame symbolized, which goes on growing:
    /// what a live view shows. Records held back stay held back.
    pub fn snapshot(&mut self) -> Capture {
        self.symbolize_new();
        let mut builder = self.builder.clone();
        self.close(&mut builder, self.latest);
        self.cut(&mut builder);
        builder.finish()
    }

    /// The finished capture, every frame symbolized.
    pub fn finish(mut self) -> Capture {
        // Everything held back is processed now.
        self.ingest(&[], None);
        self.symbolize_new();
        let mut builder = std::mem::take(&mut self.builder);
        self.close(&mut builder, self.latest);
        self.cut(&mut builder);
        builder.finish()
    }

    /// The segment since the last, or since the recording began, up to
    /// where every record has been processed, which begins the next: a
    /// complete capture of that span alone, every frame symbolized. What
    /// spans the cut, a thread off its CPU or an open zone, is in each
    /// segment for the part of it there. The events' counts must be
    /// current.
    pub fn segment(&mut self) -> Capture {
        self.symbolize_new();
        let end = self.processed;
        let mut builder = self.builder.clone();
        self.close(&mut builder, end);
        let counts = builder.accounts().clone();
        self.cut(&mut builder);
        builder.forget_from(end.cast_signed());
        builder.setting("end", &end.to_string());
        self.builder.forget_before(end.cast_signed());
        self.segment = Some((end, counts));
        self.first = Some(end);
        builder.finish()
    }

    /// Ends each interval of a thread still off its CPU, and each zone
    /// still open, at `end`, and charges the CPU time recording took.
    fn close(&self, builder: &mut CaptureBuilder, end: u64) {
        self.stacks.close(builder, end.cast_signed());
        if let Some(interpreted) = &self.interpreted {
            interpreted.close(builder);
        }

        for &(thread, left, preempted) in self.switched_out.values() {
            let back = end.min(self.next_gap(left));
            builder.off_cpu(
                thread,
                left.cast_signed(),
                back.saturating_sub(left),
                preempted,
            );
        }

        let nanoseconds = |time: std::time::Duration| u64::try_from(time.as_nanos()).unwrap_or(0);
        for (kind, total) in [
            (
                AccountKind::RecorderCpu,
                thread_cpu().saturating_sub(self.began),
            ),
            (AccountKind::RecorderLoading, self.loading),
        ] {
            let charged = builder.account(kind as u32);
            builder.count(kind as u32, nanoseconds(total).saturating_sub(charged));
        }
    }

    /// Cuts a window's rows and counts, or a segment's, to its start.
    fn cut(&self, builder: &mut CaptureBuilder) {
        if let Some((start, counts)) = self.window_start().or(self.segment.as_ref()) {
            builder.forget_before(start.cast_signed());
            builder.discount(counts);
            builder.setting("start", &start.to_string());
        }
    }

    /// Whether process `pid` has executed a program since last asked,
    /// which ends the instrumentation found before, and the modules with
    /// instrumentation it has mapped since then.
    pub fn found_probes(&mut self, pid: Pid) -> (bool, Vec<Found>) {
        let executed = self.probes_executed.remove(&pid);
        let (found, others) = std::mem::take(&mut self.probes_found)
            .into_iter()
            .partition(|(process, _)| *process == pid);
        self.probes_found = others;

        (
            executed,
            found.into_iter().map(|(_, found)| found).collect(),
        )
    }

    /// The row of a module's site.
    pub fn probe_site(&mut self, module: u32, site: &SiteRecord) -> u32 {
        self.builder
            .site(&site.name, &site.file, site.line, module, site.kind)
    }

    /// Says why some of the program's instrumentation is not recorded.
    pub fn probe_problem(&mut self, text: &str) {
        self.builder
            .marker(self.latest.cast_signed(), MarkerKind::Probes, text);
    }

    /// The row of a thread whose ring holds instrumentation.
    pub fn probe_thread(&mut self, pid: Pid, tid: Tid) -> u32 {
        self.thread(pid, tid, self.latest.cast_signed())
    }

    /// Says that a thread's records were lost before its next.
    pub fn probe_lost(&mut self, thread: u32) {
        self.stacks.lost(thread);
    }

    /// Applies one of a thread's instrumentation events, in its order.
    pub fn probe_event(&mut self, thread: u32, event: &Event<'_>) {
        self.triggered |= event.kind == microscope_probes::abi::TRIGGER;
        self.stacks.apply(&mut self.builder, thread, event);
    }

    /// Ends a thread's open zones where its records end.
    pub fn probe_thread_ended(&mut self, thread: u32) {
        self.stacks.ended(&mut self.builder, thread);
    }

    /// Whether the program asked for the window to be saved since last
    /// asked.
    pub const fn take_triggered(&mut self) -> bool {
        std::mem::replace(&mut self.triggered, false)
    }

    /// Says that process `pid` has exited, once every record of it has
    /// been processed. Each of its threads came back to its CPU to exit,
    /// so one still off it came back in records a full ring dropped, at a
    /// time no interval can say.
    pub fn exited(&mut self, pid: Pid) {
        if let Some(interpreted) = &mut self.interpreted {
            interpreted.forget(pid);
        }

        if let Some(process) = self.processes.get(&pid) {
            for tid in &process.threads {
                self.switched_out.remove(tid);
            }
        }
    }

    /// Records that every event was off from `start` to `end`, and why. A
    /// thread that left its CPU before a gap may have come back unseen in
    /// it, so its interval ends where the gap begins.
    ///
    /// A budget pause is recorded when it ends, after the task snapshots
    /// taken in it recorded theirs, so it keeps only the spans around them.
    pub fn gap(&mut self, start: u64, end: u64, reason: GapReason) {
        // A clock that failed reads as zero.
        if end <= start {
            return;
        }

        let first = self.gaps.partition_point(|&(_, earlier)| earlier <= start);
        let last = self.gaps.partition_point(|&(later, _)| later < end);
        let mut from = start;
        let mut pieces = Vec::new();

        for &(covered, until) in &self.gaps[first..last] {
            if from < covered {
                pieces.push((from, covered));
            }

            from = from.max(until);
        }

        if from < end {
            pieces.push((from, end));
        }

        for &(start, end) in &pieces {
            self.builder
                .gap(start.cast_signed(), end.cast_signed(), reason);
        }

        let mut merged = self.gaps[first..last].to_vec();
        merged.extend_from_slice(&pieces);
        merged.sort_unstable();
        self.gaps.splice(first..last, merged);
        self.gaps_end = self.gaps_end.max(end);
        debug_assert!(
            self.gaps.windows(2).all(|pair| pair[0].1 <= pair[1].0),
            "the gaps overlap after adding {start}..{end}"
        );
    }

    /// Notes the task a sample of process `pid` ran, when a task snapshot
    /// may name it: the value of the register its innermost frame's code
    /// keeps the runtime's current task in.
    fn note_task(
        &mut self,
        pid: Pid,
        thread: u32,
        time: i64,
        leaf: &crate::unwind::sample::SampledFrame,
        owners: &[Mapped],
    ) {
        if self.tasks_of != Some(pid) {
            return;
        }

        let Some(owner) = leaf.module.map(|module| owners[module]) else {
            return;
        };

        let address = owner.loaded.load_bias.wrapping_unload(leaf.address);
        let register = self.images[owner.image]
            .info
            .as_ref()
            .and_then(|info| info.unwind.task_register(address));
        let value = register.and_then(|register| {
            self.registers
                .iter()
                .find_map(|(number, value)| (*number == register).then_some(*value))
        });
        if let Some(object) = value {
            self.unclaimed.push((thread, time, object));
        }
    }

    /// Records a task snapshot that held the program stopped from `start`
    /// to `end`: each waiting task's stack, standing for `weight`
    /// nanoseconds of waiting.
    pub fn task_snapshot(
        &mut self,
        pid: Pid,
        (start, end): (u64, u64),
        weight: u64,
        snapshot: &Snapshot,
    ) {
        self.gap(start, end, GapReason::TaskSnapshot);
        self.builder
            .count(AccountKind::SnapshotPause as u32, end.saturating_sub(start));
        self.tasks(pid, (start, end), weight, snapshot);
    }

    /// Records a task snapshot read from `start` to `end` while the
    /// program ran.
    pub fn live_task_snapshot(
        &mut self,
        pid: Pid,
        (start, end): (u64, u64),
        weight: u64,
        snapshot: &Snapshot,
    ) {
        self.builder.count(AccountKind::LiveSnapshots as u32, 1);
        self.builder
            .count(AccountKind::SnapshotChanged as u32, snapshot.changed);
        self.tasks(pid, (start, end), weight, snapshot);
    }

    /// Records a snapshot's tasks and problems.
    fn tasks(&mut self, pid: Pid, (start, end): (u64, u64), weight: u64, snapshot: &Snapshot) {
        self.builder.count(AccountKind::Snapshots as u32, 1);
        if let Some(first) = snapshot.problems.first() {
            let more = snapshot.problems.len() - 1;
            let text = match more {
                0 => first.clone(),
                _ => format!("{first}, and {more} more"),
            };

            self.snapshot_failed(start, &text);
            let more = u64::try_from(more).unwrap_or(u64::MAX);
            self.builder
                .count(AccountKind::SnapshotFailures as u32, more);
        }

        let time = start.cast_signed();
        self.claim(pid, (time, end.cast_signed()), snapshot);

        // A thread's labels, as sampled, stand for the time since the
        // snapshot before, as its waits do.
        let since = time.saturating_sub(weight.cast_signed());
        for (tid, labels) in &snapshot.thread_labels {
            let thread = self.thread(pid, Tid::new(*tid), time);
            for (key, value) in labels {
                let site = self
                    .builder
                    .site(key, "", 0, NONE, microscope_probes::abi::SITE_LABEL);
                let value = self.builder.string(value);
                self.builder.zone(
                    thread,
                    Zone {
                        start: since.into(),
                        end: time.into(),
                        site: site.into(),
                        flags: 0.into(),
                        value: value.into(),
                        reserved: 0.into(),
                    },
                );
            }
        }

        for task in &snapshot.waiting {
            self.frames.clear();

            for frame in &task.frames {
                let id = match frame {
                    WaitFrame::Code { address, role } => self.code_frame(pid, *address, *role),
                    WaitFrame::Awaited(text) => self.builder.awaited(text),
                    WaitFrame::Interpreted(frame) => self.builder.interpreted(&InterpretedPlace {
                        function: &frame.function,
                        file: frame.file.as_deref(),
                        line: frame.line,
                        declared: frame.first_line,
                    }),
                    WaitFrame::Truncated(reason) => self.builder.truncation(*reason, NONE),
                };

                self.frames.push(id);
            }

            let stack = self.builder.stack(&self.frames);
            let row = self.builder.task(task.noun, task.number, time);
            if task.labels.is_some() {
                self.builder
                    .describe_task(row, NONE, task.labels.as_deref());
            }

            self.builder
                .task_wait(row, time, weight, stack, &task.state);
        }
    }

    /// Keeps where a snapshot that held the program stopped from `start`
    /// to `end` found each task, and gives each sample it follows the task
    /// its register named, when the snapshots either side of the sample
    /// find that task where it was: the runtime may reuse a task's memory
    /// for another once it exits.
    fn claim(&mut self, pid: Pid, (start, end): (i64, i64), snapshot: &Snapshot) {
        let mut tasks = HashMap::default();

        for task in &snapshot.located {
            let row = self.builder.task(task.noun, task.number, start);
            if task.labels.is_some() {
                self.builder
                    .describe_task(row, NONE, task.labels.as_deref());
            }

            tasks.insert(task.object, row);
        }

        if self.located.len() == LOCATED_KEPT {
            self.located.pop_front();
        }

        self.located.push_back(Located { start, end, tasks });
        self.tasks_of = Some(pid);
        let mut claimed = 0;
        let mut waiting = Vec::new();

        for (thread, at, object) in std::mem::take(&mut self.unclaimed) {
            if at >= start {
                waiting.push((thread, at, object));
                continue;
            }

            // A sample older than every snapshot kept is never claimed.
            let before = self.located.iter().rev().find(|located| located.end <= at);
            let after = self.located.iter().find(|located| located.start > at);
            if let (Some(before), Some(after)) = (before, after)
                && let Some(&task) = before.tasks.get(&object)
                && after.tasks.get(&object) == Some(&task)
            {
                claimed += self.builder.claim(thread, at, task);
            }
        }

        self.unclaimed = waiting;
        self.builder.count(AccountKind::TaskSamples as u32, claimed);
    }

    /// Each piece of process `pid`'s code, in the order a frame's code is
    /// found among them.
    pub fn mapped_code(&self, pid: Pid) -> Vec<super::tasks::MappedCode> {
        self.processes.get(&pid).map_or_else(Vec::new, |process| {
            process
                .mapped
                .iter()
                .map(|mapped| super::tasks::MappedCode {
                    start: mapped.start,
                    end: mapped.end,
                    image: mapped.loaded.image,
                    bias: mapped.loaded.load_bias,
                    generated: self
                        .images
                        .get(mapped.image)
                        .is_some_and(|image| image.generated),
                })
                .collect()
        })
    }

    /// The modules process `pid` maps, as its mapping records last said,
    /// each once, with its debug information: what a live task snapshot
    /// reads the process's runtimes through. A module whose debug
    /// information could not be read is left out.
    pub fn modules_of(&self, pid: Pid) -> Vec<(LoadedModule, DebugInfo)> {
        self.processes
            .get(&pid)
            .map_or_else(Vec::new, |process| modules_in(process, &self.images))
    }

    /// Records that a task snapshot at `time` failed, or missed tasks, and
    /// why.
    pub fn snapshot_failed(&mut self, time: u64, why: &str) {
        record!("record: {why}");
        self.builder.count(AccountKind::SnapshotFailures as u32, 1);
        self.builder
            .marker(time.cast_signed(), MarkerKind::TaskSnapshot, why);
    }

    /// The frame of the code at `address` in process `pid`.
    fn code_frame(&mut self, pid: Pid, address: u64, role: u8) -> u32 {
        let owner = self.processes.get(&pid).and_then(|process| {
            process
                .mapped
                .iter()
                .find(|mapped| mapped.start <= address && address < mapped.end)
                .copied()
        });
        let owner = owner.filter(|owner| self.names(owner, address));
        // The debugger knew the role of code in no module the recording
        // knows, as V8's: it keeps it.
        let (module, address, role) = owner.map_or((NONE, address, role), |owner| {
            (
                self.images[owner.image].module,
                owner
                    .loaded
                    .load_bias
                    .wrapping_unload(crate::VirtualAddress::new(address))
                    .get(),
                role,
            )
        });
        self.builder.frame(FrameKind::Native, module, address, role)
    }

    /// Whether a frame at `address` is one of `owner`'s module: anywhere in
    /// a file's, and only in a function it names in anonymous code's.
    fn names(&self, owner: &Mapped, address: u64) -> bool {
        let image = &self.images[owner.image];
        !image.anonymous
            || image
                .info
                .as_ref()
                .is_some_and(|info| info.image.symbolize(ImageAddress::new(address)).is_some())
    }

    /// Where the first gap after `time` began, or the end of time.
    fn next_gap(&self, time: u64) -> u64 {
        let next = self.gaps.partition_point(|&(start, _)| start <= time);
        self.gaps.get(next).map_or(u64::MAX, |&(start, _)| start)
    }

    /// The CPU time loading images has taken.
    pub const fn loading(&self) -> std::time::Duration {
        self.loading
    }

    /// Leaves every frame without lines, for a viewer to symbolize from
    /// its module's build id and address.
    pub fn symbols_later(&mut self) {
        self.symbols_later = true;
        self.builder.setting("symbols", "later");
    }

    /// Gives each frame not yet symbolized its lines, and the capture its
    /// span so far.
    fn symbolize_new(&mut self) {
        let start = self.first.unwrap_or(0);
        self.builder.setting("start", &start.to_string());

        // A segment cut after the last record ends no earlier than it began.
        let end = self.latest.max(self.gaps_end).max(start);
        self.builder.setting("end", &end.to_string());

        // A viewer finds a file's module to symbolize it later; anonymous
        // code is gone by then.
        let images: BTreeMap<u32, usize> = self
            .images
            .iter()
            .enumerate()
            .filter(|(_, image)| image.anonymous || !self.symbols_later)
            .map(|(index, image)| (image.module, index))
            .collect();
        if images.is_empty() {
            return;
        }

        let frames: Vec<(u32, u32, u64)> = self
            .builder
            .frames()
            .iter()
            .enumerate()
            .filter(|(index, frame)| {
                frame.kind == FrameKind::Native as u8
                    && frame.module.get() != NONE
                    && !self
                        .builder
                        .symbolized(u32::try_from(*index).unwrap_or(u32::MAX))
            })
            .map(|(index, frame)| {
                (
                    u32::try_from(index).unwrap_or(u32::MAX),
                    frame.module.get(),
                    frame.address.get(),
                )
            })
            .collect();

        for (frame, module, address) in frames {
            let Some(info) = images
                .get(&module)
                .and_then(|&image| self.images[image].info.as_ref())
            else {
                continue;
            };

            let lines = symbolize(&info.image, ImageAddress::new(address));
            let inputs: Vec<LineInput<'_>> = lines.iter().map(OwnedLine::input).collect();
            self.builder.symbolize(frame, module, &inputs);
        }
    }
}

/// One source line of a frame, owned until the builder interns it.
pub struct OwnedLine {
    function: Arc<str>,
    file: Option<String>,
    line: u32,
    column: u32,
    declared: Option<(String, u32)>,
    role: CodeRole,
}

impl OwnedLine {
    pub fn input(&self) -> LineInput<'_> {
        LineInput {
            function: &self.function,
            file: self.file.as_deref(),
            line: self.line,
            column: self.column,
            declared: self
                .declared
                .as_ref()
                .map(|(file, line)| (file.as_str(), *line)),
            role: role_number(self.role),
        }
    }
}

/// The source lines of the code at `address`, innermost inlined function
/// first, ending with the function the code physically belongs to.
pub fn symbolize(image: &ModuleImage, address: ImageAddress) -> Vec<OwnedLine> {
    let location = image.locate(address);
    let path = |file| {
        image
            .source_file(file)
            .map(|file: &crate::SourceFile| file.path.display().to_string())
    };

    let line = |function: Arc<str>,
                role: CodeRole,
                declaration: Option<&crate::SourceLocation>,
                source: Option<&crate::SourceLocation>| OwnedLine {
        function,
        role,
        file: source.and_then(|source| path(source.file)),
        line: source.map_or(0, |source| {
            u32::try_from(source.line.get()).unwrap_or(u32::MAX)
        }),
        column: source
            .and_then(|source| source.column)
            .map_or(0, |column| u32::try_from(column.get()).unwrap_or(u32::MAX)),
        declared: declaration.and_then(|declaration| {
            Some((
                path(declaration.file)?,
                u32::try_from(declaration.line.get()).unwrap_or(u32::MAX),
            ))
        }),
    };

    let mut lines = Vec::new();
    let mut source = location.source.clone();
    if let InlineFrameLookup::Unique(chain) = &location.inline_frames {
        for &instance in chain.instances.iter().rev() {
            let Some(instance) = image.code_instance(instance) else {
                continue;
            };

            if let Some(function) = image.function(instance.function()) {
                let function = function.info();
                lines.push(line(
                    qualified(&function),
                    function.role,
                    function.declaration.as_ref(),
                    source.as_ref(),
                ));
            }

            source = match instance.kind() {
                CodeInstanceKind::Inline { call_site } => call_site,
                CodeInstanceKind::OutOfLine => None,
            };
        }
    }

    let physical = location
        .physical_instance
        .and_then(|instance| image.code_instance(instance))
        .and_then(|instance| image.function(instance.function()))
        .map(crate::Function::info);
    if let Some(function) = physical {
        lines.push(line(
            qualified(&function),
            function.role,
            function.declaration.as_ref(),
            source.as_ref(),
        ));
    } else if let Some(symbol) = &location.symbol {
        let found = image.symbol(symbol.symbol);
        let name = found
            .and_then(crate::Symbol::demangled_name)
            .map_or_else(|| symbol.name.clone(), Arc::from);
        let role = found.map_or(CodeRole::Ordinary, crate::Symbol::role);
        lines.push(line(name, role, None, source.as_ref()));
    }

    lines
}

/// A function's name with the scopes that qualify it, as its linkage name
/// spells them: a profile names every function of a program side by side.
fn qualified(function: &crate::FunctionInfo) -> Arc<str> {
    function
        .linkage_name
        .as_deref()
        .and_then(crate::demangle::qualified_name)
        .map_or_else(|| function.name.clone(), Arc::from)
}

pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

/// Truncations by number, for the accounts that count them.
pub fn truncation_counts(capture: &Capture) -> Vec<(Truncation, u64)> {
    Truncation::ALL
        .into_iter()
        .map(|reason| {
            (
                reason,
                capture.account(AccountKind::Truncated as u32 + reason as u32),
            )
        })
        .filter(|(_, count)| *count > 0)
        .collect()
}

/// The CPU time this thread has taken.
pub fn thread_cpu() -> std::time::Duration {
    nix::time::clock_gettime(nix::time::ClockId::CLOCK_THREAD_CPUTIME_ID).map_or(
        std::time::Duration::ZERO,
        |time| {
            std::time::Duration::new(
                u64::try_from(time.tv_sec()).unwrap_or(0),
                u32::try_from(time.tv_nsec()).unwrap_or(0),
            )
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A segment cut where every record was processed, with nothing
    /// recorded after it, ends no earlier than it begins.
    #[test]
    fn a_last_segment_with_nothing_after_its_cut_ends_where_it_begins() {
        let mut processor = Processor::new(DebugFileSearch::default(), 64);
        processor.ingest(&[], Some(5_000));
        let first = processor.segment();
        let last = processor.finish();
        assert_eq!(first.span().1, 5_000);
        assert_eq!(last.span(), (5_000, 5_000));
    }

    /// A full ring drops whatever does not fit, so a thread can come back
    /// to its CPU and exit unseen. Once its process has exited, the thread
    /// must have, at a time nothing recorded: no interval says when. A
    /// thread of a process that lives on is still off its CPU at the end.
    #[test]
    fn a_thread_whose_return_was_dropped_has_no_interval() {
        let mut processor = Processor::new(DebugFileSearch::default(), 64);
        let (exited, napper, living, sleeper) =
            (Pid::new(100), Tid::new(101), Pid::new(200), Tid::new(201));
        for record in [
            decode::Record::Fork {
                pid: exited,
                ppid: Pid::new(1),
                tid: exited.leader(),
                time: 1_000,
            },
            decode::Record::Fork {
                pid: exited,
                ppid: exited,
                tid: napper,
                time: 2_000,
            },
            decode::Record::Fork {
                pid: living,
                ppid: Pid::new(1),
                tid: living.leader(),
                time: 2_000,
            },
            decode::Record::Fork {
                pid: living,
                ppid: living,
                tid: sleeper,
                time: 2_000,
            },
            decode::Record::Switch {
                pid: exited,
                tid: napper,
                out: true,
                preempted: false,
                time: 3_000,
            },
            decode::Record::Switch {
                pid: living,
                tid: sleeper,
                out: true,
                preempted: false,
                time: 4_000,
            },
            // The napper's return and exit were dropped; the leader's
            // exit was not.
            decode::Record::Exit {
                pid: exited,
                tid: exited.leader(),
                time: 9_000,
            },
        ] {
            processor.process(record);
        }

        processor.exited(exited);
        let profile = super::super::Profile {
            capture: processor.finish(),
        };

        let intervals: Vec<_> = profile
            .off_cpu()
            .iter()
            .map(|interval| (interval.tid, interval.time, interval.duration))
            .collect();
        assert_eq!(intervals, [(sleeper.get(), 4_000, 5_000)]);
    }

    /// A mapping over part of a library, as `mmap(MAP_FIXED)` over one of
    /// its pages makes, takes only that part: the library's other pages
    /// still name its code.
    #[test]
    fn a_mapping_over_part_of_a_module_leaves_the_rest_of_it() {
        use object::{Object, ObjectSegment};
        use std::os::unix::fs::MetadataExt;

        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("build/test-programs/libcrash.so")
            .canonicalize()
            .expect("run `just build-test-programs`");
        let data = std::fs::read(&path).expect("read the library");
        let file = object::File::parse(data.as_slice()).expect("an ELF file");
        let (offset, size) = file
            .segments()
            .find(|segment| {
                matches!(
                    segment.flags(),
                    object::SegmentFlags::Elf { p_flags } if p_flags & object::elf::PF_X != 0
                )
            })
            .map(|segment| segment.file_range())
            .expect("an executable segment");
        let (offset, length) = (offset & !0xfff, (size + 0x2fff) & !0xfff);
        let start = 0x7f00_0000_0000 + offset;
        let pid = Pid::new(7);
        let mut processor = Processor::new(DebugFileSearch::default(), 64);
        let inode = std::fs::metadata(&path).expect("the library's inode").ino();
        let path = path.as_os_str().as_encoded_bytes().to_vec();

        processor.process(decode::Record::Mmap(decode::Mmap {
            pid,
            start,
            length,
            file_offset: offset,
            inode: Some(inode),
            executable: true,
            path: &path,
            time: 1_000,
        }));
        let module = processor.processes[&pid].mapped.clone();
        assert_eq!(module.len(), 1, "{module:?}");

        processor.process(decode::Record::Mmap(decode::Mmap {
            pid,
            start: start + 0x1000,
            length: 0x1000,
            file_offset: 0,
            inode: None,
            executable: false,
            path: b"//anon",
            time: 2_000,
        }));
        let pieces = processor.processes[&pid]
            .mapped
            .iter()
            .map(|mapped| (mapped.start, mapped.end, mapped.image, mapped.loaded))
            .collect::<Vec<_>>();
        let (image, loaded) = (module[0].image, module[0].loaded);
        assert_eq!(
            pieces,
            [
                (start, start + 0x1000, image, loaded),
                (start + 0x2000, start + length, image, loaded),
            ]
        );
    }

    /// A module whose mapping was lost with a full ring's records is read
    /// from the process's mappings, with its instrumentation, once every
    /// record before they were read is processed, and is read only once.
    #[test]
    fn a_module_whose_mapping_record_was_lost_is_found_in_the_process() {
        use std::io::{BufRead as _, Write as _};

        let program = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("build/test-programs/probes")
            .canonicalize()
            .expect("run `just build-test-programs`");
        let mut child = std::process::Command::new(&program)
            .arg("defines")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("the program");
        let pid = Pid::new(child.id());

        // Once it answers, the loader has mapped every library it needs.
        let mut names = child.stdin.take().expect("its input");
        let mut lines = std::io::BufReader::new(child.stdout.take().expect("its output")).lines();
        writeln!(names, "ready").expect("name a site");
        lines.next().expect("a site").expect("its line");
        let mut processor = Processor::new(DebugFileSearch::default(), 64);
        let comm = |time| decode::Record::Comm {
            pid,
            tid: Tid::new(pid.get()),
            name: b"probes",
            exec: false,
            time,
        };

        processor.process(comm(1_000));
        processor.process(decode::Record::Lost {
            count: 1,
            time: 2_000,
        });
        processor.process(comm(3_000));
        let early = processor.found_probes(pid).1.len();

        processor.ingest(&[], None);
        let found = processor.found_probes(pid).1;
        let mapped = processor.processes[&pid].mapped.len();

        processor.process(decode::Record::Lost {
            count: 1,
            time: super::super::session::monotonic(),
        });
        processor.ingest(&[], None);
        let again = (
            processor.found_probes(pid).1.len(),
            processor.processes[&pid].mapped.len(),
        );

        drop(names);
        child.wait().expect("its exit");
        assert_eq!(early, 0, "mapped before the records before the read");
        assert_eq!(
            found
                .iter()
                .map(|found| std::path::Path::new(&*found.path))
                .collect::<Vec<_>>(),
            [program.as_path()]
        );
        assert!(mapped > 0);
        assert_eq!(again, (0, mapped), "a known mapping was mapped again");
    }

    /// The bytes of a perf sample record that event `identifier` wrote of
    /// thread 8 of process 7 on CPU 3 at `time`, followed by `rest`.
    fn perf_record(identifier: u64, time: u64, rest: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();

        for word in [identifier, 7 | (8 << 32), time, 3] {
            body.extend_from_slice(&word.to_le_bytes());
        }

        body.extend_from_slice(rest);
        let mut record = 9_u32.to_le_bytes().to_vec();
        record.extend_from_slice(&0_u16.to_le_bytes());
        record.extend_from_slice(&u16::try_from(8 + body.len()).expect("small").to_le_bytes());
        record.extend_from_slice(&body);
        record
    }

    /// A sample with no chain, registers, or stack.
    fn bare_sample(time: u64) -> Vec<u8> {
        let mut rest = Vec::new();

        for word in [1_000_u64, 0, 0, 0] {
            rest.extend_from_slice(&word.to_le_bytes());
        }

        perf_record(1, time, &rest)
    }

    /// An output event's record of a thread with no frames, which holds
    /// its GIL or not, and runs the asyncio task at `task`, if not 0.
    fn frames_record(time: u64, holds: bool, task: u64) -> Vec<u8> {
        let mut stack = Vec::new();

        for word in [9_u32, 40, 8, 7] {
            stack.extend_from_slice(&word.to_le_bytes());
        }

        stack.extend_from_slice(&0_u64.to_le_bytes());
        stack.extend_from_slice(&task.to_le_bytes());
        stack.extend_from_slice(&0_u32.to_le_bytes());
        stack.extend_from_slice(&[4, if holds { 1 } else { 3 }, 0, 0]);

        let mut rest = u32::try_from(stack.len() + 4)
            .expect("small")
            .to_le_bytes()
            .to_vec();
        rest.extend_from_slice(&stack);
        rest.extend_from_slice(&[0; 4]);
        perf_record(42, time, &rest)
    }

    /// The spans of the GIL label a capture holds.
    fn gil_spans(capture: &Capture) -> Vec<(i64, i64)> {
        let mut spans: Vec<(i64, i64)> = capture
            .zones
            .iter()
            .map(|zone| (zone.start.get(), zone.end.get()))
            .collect();
        spans.sort_unstable();
        spans
    }

    /// A record of Python frames goes with the next sample of its thread
    /// and CPU in the ring, whichever is stamped first and in whichever
    /// drain it comes: held back with its sample past the watermark, and
    /// waiting for a sample a later drain brings. One whose sample never
    /// comes was dropped with it, and gives no later sample its frames.
    #[test]
    fn each_record_of_python_frames_goes_with_the_next_sample_of_its_thread() {
        let mut processor = Processor::new(DebugFileSearch::default(), 64);
        processor.interpret(vec![42]);

        // The record stamped after its sample, as newer kernels stamp them.
        let mut batch = frames_record(10_500, true, 0);
        batch.extend(bare_sample(10_000));
        processor.ingest(&batch, Some(50_000_000));

        // A record whose sample the next drain brings.
        processor.ingest(&frames_record(100_000_000, true, 0), Some(95_000_000));
        processor.ingest(&bare_sample(100_000_000), Some(150_000_000));

        // A record and its sample both past the watermark.
        let mut batch = frames_record(200_000_000, true, 0);
        batch.extend(bare_sample(200_000_000));
        processor.ingest(&batch, Some(199_000_000));
        processor.ingest(&[], Some(250_000_000));

        // A record whose sample was dropped, then a sample with no record.
        processor.ingest(&frames_record(300_000_000, true, 0), Some(301_000_000));
        processor.ingest(&[], Some(400_000_000));
        processor.ingest(&bare_sample(400_000_000), Some(450_000_000));

        let capture = processor.finish();
        assert_eq!(
            gil_spans(&capture),
            [
                (9_000, 10_000),
                (99_999_000, 100_000_000),
                (199_999_000, 200_000_000),
            ]
        );
    }

    /// A sample whose record names the asyncio task its thread ran is
    /// claimed for that task when the snapshots on both sides of it locate
    /// the task at that object; one that names another task is not.
    #[test]
    fn a_sample_is_claimed_for_the_asyncio_task_its_record_names() {
        let mut processor = Processor::new(DebugFileSearch::default(), 64);
        processor.interpret(vec![42]);
        let located = |object| Snapshot {
            located: vec![crate::profiler::tasks::Located {
                object,
                noun: "task",
                number: object,
                labels: None,
            }],
            ..Snapshot::default()
        };

        let pid = Pid::new(7);
        processor.task_snapshot(pid, (1_000, 2_000), 0, &located(0x5000));
        let mut batch = frames_record(10_000, false, 0x5000);
        batch.extend(bare_sample(10_000));
        batch.extend(frames_record(11_000, false, 0x6000));
        batch.extend(bare_sample(11_000));
        processor.ingest(&batch, Some(50_000));
        processor.task_snapshot(pid, (60_000, 61_000), 0, &located(0x5000));
        let capture = processor.finish();
        assert_eq!(capture.account(AccountKind::TaskSamples as u32), 1);
    }
}
