//! Turns the records perf events write into a capture: it follows each
//! process's mappings and threads, unwinds each sample through the modules
//! mapped when it was taken, and symbolizes the frames once at the end.

mod cuts;
mod held;
mod modules;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use foldhash::HashMap;

use super::Cuts;
use super::calls::Branch;
use super::collector::{Found, FoundProbes};
use super::frame_pointers::{CallSites, call_sites};
use super::interpreted::{self, Interpreted, Sampled, Walked};
use super::jit::{Jit, JitCode, Stale};
use super::probes::{self, Event, SiteRecord, Stacks};
use super::tasks::{InterpretedWait, Snapshot, WaitFrame};
use crate::backend::perf::{self, decode};
use crate::capture::builder::LineInput;
use crate::capture::records::{
    AccountKind, FrameKind, GapReason, Mapping, MarkerKind, NONE, Truncation, Zone, role_number,
};
use cuts::{Cut, End};
use held::{Batches, RecordAt};
use modules::{Mappings, ModuleIndex};

use crate::capture::{Capture, CaptureBuilder};
use crate::debug_info::{DebugFileSearch, DebugInfo, gdb_jit};
use crate::model::{Pid, Tid};
use crate::unwind::sample::{FrameRules, SampledFrame, SampledState, StackEnd, unwind_sample_into};
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
/// How far behind a live read's watermark the records processed may lag
/// before the read drains the rings first, in nanoseconds: the drain just
/// before a read on time leaves them within it.
const READ_SLACK: u64 = 2_000_000;

/// The most drains a live read takes before it.
const READ_DRAINS: u32 = 4;

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
    image.v8.then(|| {
        interpreted::CodeAt::Runtime(
            mapped
                .loaded
                .load_bias
                .wrapping_unload(crate::VirtualAddress::new(address)),
        )
    })
}

/// Records of interpreters' frames waiting for their sample: by thread and
/// CPU, when, and where among a drain's records.
type Unpaired = Vec<((Tid, u32), u64, usize)>;

/// A native frame as the capture keeps it, with the name of the
/// interpreter's code it runs, if it runs the interpreter's.
type NamedFrame = (u32, Option<Arc<str>>);

/// What processing a sample builds, kept from sample to sample so that
/// processing one allocates nothing.
#[derive(Debug, Default)]
struct SampleScratch {
    frames: Vec<SampledFrame>,

    /// Each native frame as the capture keeps it, with the name of the
    /// interpreter's code it runs, if it runs the interpreter's.
    named: Vec<NamedFrame>,

    /// Each native frame's module and address, its key.
    keys: Vec<(u32, u64)>,

    /// Each native frame, with whether it is an activation of an
    /// interpreter's loop.
    native: Vec<(u32, bool)>,

    /// The addresses the frames and the kernel's chain landed in.
    addresses: Vec<u64>,
}

/// How a program's call sites keep the frame-pointer chain.
pub enum Sites {
    /// It is linked dynamically, so its code is not all in its file.
    Dynamic,

    /// Its own file's, with the function of its first call that does not.
    Static(CallSites, Option<String>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DebugLoading {
    Full,
    AfterCollection,
}

/// Image `index`. The recorder only pushes images, and every index it
/// keeps is one it pushed.
#[expect(
    clippy::indexing_slicing,
    reason = "images are only pushed, and every index kept is one pushed"
)]
fn image_of(images: &[Image], index: usize) -> &Image {
    debug_assert!(index < images.len(), "image {index} of {}", images.len());
    &images[index]
}

#[expect(
    clippy::indexing_slicing,
    reason = "images are only pushed, and every index kept is one pushed"
)]
fn image_of_mut(images: &mut [Image], index: usize) -> &mut Image {
    debug_assert!(index < images.len(), "image {index} of {}", images.len());
    &mut images[index]
}

/// Record `index` of a drain's records. The drain keeps only indexes of
/// records it already pushed.
#[expect(
    clippy::indexing_slicing,
    reason = "a drain keeps only indexes of records it already pushed"
)]
fn record_at(records: &[RecordAt], index: usize) -> RecordAt {
    debug_assert!(index < records.len(), "record {index} of {}", records.len());
    records[index]
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

    /// The file identity for full metadata deferred until collection ends.
    deferred: Option<([u8; 16], Option<String>)>,

    /// Whether it is code a runtime generated, as V8's compiled
    /// JavaScript, which the runtime's frames run.
    generated: bool,

    /// Whether it is V8's (`debug_info::v8::is_v8`), told once: the test
    /// looks a symbol up by name, and samples ask it of each frame.
    v8: bool,

    /// The runtimes it carries that keep code of their own in a process,
    /// detected the first time a mapping may copy its code: V8 copies its
    /// builtins once for each isolate.
    code_keepers: std::sync::OnceLock<Vec<Arc<dyn crate::runtime_model::RuntimeModel>>>,

    /// The range of its code the last copy proven of it began with. V8
    /// copies the same builtins for each isolate, which this finds without
    /// probing every function that begins a page.
    copied: Option<(ImageAddress, ImageAddress)>,

    /// What loading its debug information needs, until it is loaded.
    pending: Option<Pending>,
}

/// An image whose debug information is still to be loaded: until it is,
/// `info` is `None` and no mapping of it is processed.
struct Pending {
    path: PathBuf,
    data: Option<Vec<u8>>,
    digest: [u8; 16],
    build_id: Option<String>,
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
    mapped: Mappings,

    /// How many mappings the process has made of a module, or over part
    /// of one, which tells its modules apart from what they were, as their
    /// number may not. Anonymous code, which a JIT compiler maps over and
    /// over, and the modules made of it, change none.
    mappings: u64,

    /// How many times the process has exec'd.
    execs: u64,

    /// Whether its command line has been refreshed at its first sample
    /// since it began or last exec'd.
    command_read: bool,
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

/// When a drain loads the debug information of a module it finds mapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Loads {
    /// As it processes the mapping.
    AsMapped,

    /// Once the collector has read the instrumentation of every module
    /// mapped so far: the drain stops at the mapping.
    AfterProbes,
}

/// Each sample's record of its interpreter's frames, by the sample's place.
type Pairs = HashMap<usize, (interpreted::RecordKey, usize)>;

/// A drain's records in the order they are processed, where each lies,
/// and the samples' pairs.
type Ordered<'a> = (&'a [(u64, usize)], &'a [RecordAt], &'a Pairs);

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

    /// The batches drained, kept while records in them are held back.
    batches: Batches,

    /// Records held back for the next batch, past its watermark, in the
    /// order they go before it.
    held: Vec<RecordAt>,

    /// When this drain loads debug information (see
    /// [`Processor::ingest_drained_after_probes`]), and the image whose
    /// mapping it stopped at, if it did.
    loads: Loads,
    stopped: Option<usize>,

    /// Where each record of a batch lies, and in what order they are
    /// processed, by their time and then their place.
    records: Vec<RecordAt>,
    order: Vec<(u64, usize)>,

    /// The latest watermark records were processed up to.
    processed_to: u64,

    /// The ring heads that could not be trusted, as last counted.
    untrusted: u64,

    /// The latest time processed.
    latest: u64,
    first: Option<u64>,
    registers: Vec<(crate::unwind::DwarfRegister, u64)>,
    scratch: SampleScratch,

    /// The name of each system call a kernel sample's leaf has named.
    kernel_leaves: HashMap<Option<crate::syscalls::SyscallId>, String>,

    /// A breakpoint's instruction and the branch that makes its samples
    /// repeats, not calls.
    repeats: Option<(u64, Branch)>,
    fixed_period: Option<u64>,
    chain: Vec<u64>,
    frames: Vec<u32>,

    /// Each thread off its CPU since a switch: its row, when it left, and
    /// whether it was preempted.
    switched_out: BTreeMap<Tid, (u32, u64, bool)>,

    /// Each gap in sampling, in time order and apart, and where the last
    /// one ended: a recording that ends paused ends there.
    gaps: Vec<(u64, u64)>,
    gaps_end: u64,

    /// Where a budget pause that is not yet a gap began.
    paused: Option<u64>,

    /// The CPU time of this thread when the processor began, and what
    /// loading images has taken of it since.
    began: std::time::Duration,
    setup: Option<std::time::Duration>,
    loading: std::time::Duration,
    collection: std::time::Duration,

    /// What the loader's workers had spent when the processor began.
    pool_began: std::time::Duration,

    /// How many threads have started, ended, run a new program, or mapped
    /// a module, which may be an interpreter's, so that what the engine
    /// was told of them can be told again only then.
    thread_changes: u64,

    /// The time every record up to has been processed.
    processed: u64,
    cut: Cut,

    /// The process task snapshots read, the last few snapshots, and each
    /// sample no snapshot after it has claimed yet whose code kept its task
    /// in a register: its thread, time, and the register's value.
    tasks_of: Option<Pid>,
    located: std::collections::VecDeque<Located>,
    unclaimed: Vec<(u32, i64, u64)>,

    /// Each process's modules with instrumentation, once mapped since it
    /// last executed a program, those the collector has not taken yet, and
    /// the processes that executed one since the collector last asked,
    /// with the modules it found but did not take before they went.
    probes_mapped: BTreeSet<(Pid, usize)>,
    probes_found: Vec<(Pid, Found)>,
    probes_executed: BTreeMap<Pid, Vec<Found>>,

    /// The mappings of each process read after a loss, by the time they
    /// were read, which take the place of the records lost once every
    /// record before that time is processed (see [`Processor::lost`]).
    lost_mappings: std::collections::VecDeque<(u64, Vec<ProcessMappings>)>,

    /// The addresses each live read of a process found frames at in no
    /// mapping, by when the read ended, whose anonymous code is mapped
    /// once every record before then is processed (see
    /// [`Processor::map_live_code`]).
    live_unmapped: std::collections::VecDeque<(u64, Pid, Vec<u64>)>,
    stacks: Stacks,
    triggered: bool,

    /// Whether frames are left for a viewer to symbolize.
    symbols_later: bool,

    /// Ordinary native collection reads no unit DWARF on the sample path.
    debug_loading: DebugLoading,

    /// The BPF engine's Python frames, when it runs, and the record of
    /// them paired with the sample being processed.
    interpreted: Option<Interpreted>,
    paired: Option<(interpreted::RecordKey, Vec<u8>)>,

    /// The buffer the record paired with the sample before was kept in.
    paired_bytes: Vec<u8>,
}

impl Processor {
    pub fn new(search: DebugFileSearch, limit: usize) -> Self {
        Self {
            builder: CaptureBuilder::new(),
            rules: FrameRules::default(),
            search,
            images: Vec::new(),
            image_index: HashMap::default(),
            loads: Loads::AsMapped,
            stopped: None,
            vdso_read: false,
            vdso: None,
            processes: BTreeMap::new(),
            next_module: 0,
            // A stack keeps at least its innermost frame.
            limit: limit.max(1),
            batches: Batches::default(),
            held: Vec::new(),
            records: Vec::new(),
            processed_to: 0,
            untrusted: 0,
            latest: 0,
            first: None,
            order: Vec::new(),
            scratch: SampleScratch::default(),
            kernel_leaves: HashMap::default(),
            registers: Vec::with_capacity(decode::DWARF_REGISTERS.len()),
            repeats: None,
            fixed_period: None,
            chain: Vec::new(),
            frames: Vec::new(),
            switched_out: BTreeMap::new(),
            gaps: Vec::new(),
            gaps_end: 0,
            paused: None,
            began: thread_cpu(),
            setup: None,
            loading: std::time::Duration::ZERO,
            collection: std::time::Duration::ZERO,
            pool_began: crate::pool::cpu(),
            thread_changes: 0,
            processed: 0,
            cut: Cut::Whole,
            tasks_of: None,
            located: std::collections::VecDeque::new(),
            unclaimed: Vec::new(),
            probes_mapped: BTreeSet::new(),
            probes_found: Vec::new(),
            lost_mappings: std::collections::VecDeque::new(),
            live_unmapped: std::collections::VecDeque::new(),
            probes_executed: BTreeMap::new(),
            stacks: Stacks::default(),
            triggered: false,
            symbols_later: false,
            debug_loading: DebugLoading::Full,
            interpreted: None,
            paired: None,
            paired_bytes: Vec::new(),
        }
    }

    /// Splices the Python frames the BPF engine writes, through the
    /// output events whose identifiers are `outputs`, into the samples.
    pub fn interpret(&mut self, outputs: Vec<u64>) {
        self.interpreted = Some(Interpreted::new(outputs));
    }

    /// The layouts of interpreters read from mappings drained but not yet
    /// processed, since last asked.
    pub fn walked_early(&mut self) -> Vec<Walked> {
        self.interpreted
            .as_mut()
            .map_or_else(Vec::new, Interpreted::take_early)
    }

    /// Reads the layout of an interpreter `mmap` maps for the BPF engine,
    /// before the mapping is processed.
    fn mapped_early(&mut self, mmap: &decode::Mmap<'_>) {
        if let Some(interpreted) = &mut self.interpreted {
            let program = self
                .processes
                .get(&mmap.pid)
                .map_or(0, |process| process.execs);
            interpreted.mapped_early(mmap, program);
        }
    }

    /// What each process's interpreter tells the BPF engine, read now.
    pub fn walked(&mut self) -> Vec<Walked> {
        let Some(interpreted) = &mut self.interpreted else {
            return Vec::new();
        };

        let mut walked: Vec<Walked> = self
            .processes
            .iter()
            .filter_map(|(&pid, process)| {
                find_interpreter(interpreted, pid, process, &self.images);
                interpreted.walked(pid, process.execs)
            })
            .collect();

        // A process whose mappings are not yet processed has only the
        // layout read early from them.
        walked.extend(interpreted.early_unknown(|pid| self.processes.contains_key(&pid)));
        walked
    }

    /// How many threads have started, ended, run a new program, or mapped a
    /// module so far.
    pub const fn thread_changes(&self) -> u64 {
        self.thread_changes
    }

    pub const fn builder(&mut self) -> &mut CaptureBuilder {
        &mut self.builder
    }

    /// Breakpoint events omit `PERIOD` so the kernel honors their period.
    pub const fn fixed_period(&mut self, period: u64) {
        self.fixed_period = Some(period);
    }

    /// How the program at `path` keeps the frame-pointer chain, or `None`
    /// if it cannot be read. Its image is loaded as its mapping will find
    /// it, and loaded once.
    pub fn call_sites(&mut self, path: &std::path::Path) -> Option<Sites> {
        use std::os::unix::fs::MetadataExt;
        let _span = crate::span!("record call sites");
        let data = std::fs::read(path).ok()?;
        let file = object::File::parse(data.as_slice()).ok()?;
        if object::Object::section_by_name(&file, ".interp").is_some() {
            return Some(Sites::Dynamic);
        }

        let inode = std::fs::metadata(path).ok()?.ino();
        let image = self.image(path.to_owned(), path, Some(inode), false);
        let info = image_of(&self.images, image).info.as_ref()?;
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

    /// The layout the BPF walker reads the V8 of the program at `path` by,
    /// if it is one, before the program runs: its image is loaded now,
    /// and found again by its path and inode once the program maps it.
    pub fn program_v8_layout(
        &mut self,
        path: &std::path::Path,
    ) -> Option<crate::backend::bpf::sampler::RawV8Layout> {
        use std::os::unix::fs::MetadataExt;
        let inode = std::fs::metadata(path).ok()?.ino();
        let image = self.image(path.to_owned(), path, Some(inode), false);
        interpreted::v8_layout_before(image_of(&self.images, image).info.as_ref()?)
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

        self.read_command(pid, at);

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

    /// Takes the events' counts so far. A span a ring skipped because its
    /// head could not be trusted counts as a record that could not be read.
    fn counts(&mut self, counts: perf::Counts) {
        let perf::Counts {
            counted,
            lost_samples,
            lost_records,
            untrusted,
            shortened,
        } = counts;
        for (kind, total) in [
            (AccountKind::Counted, counted),
            (AccountKind::LostSamples, lost_samples),
            (AccountKind::LostRecords, lost_records),
            (AccountKind::ShortenedStacks, shortened),
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

    /// Cuts the recording as `cuts` says, before any record.
    pub fn cuts(&mut self, cuts: Cuts) {
        debug_assert!(self.first.is_none(), "cut once a record was processed");
        self.cut = Cut::new(cuts);
    }

    /// Takes the events' counts read at `time`. A cut recording counts
    /// them once every record before `time` is processed, not at the older
    /// watermark of the batch being processed then.
    pub fn observe(&mut self, time: u64, counts: perf::Counts) {
        if let Some(counts) = self.cut.observe(time, counts) {
            self.counts(counts);
        }
    }

    /// Whether the counts read at `at` have been counted, with every
    /// record before them.
    pub fn observed(&self, at: u64) -> bool {
        self.cut.observed(at)
    }

    /// Counts the counts read by `time`, and forgets what a window has
    /// passed.
    fn account_before(&mut self, time: u64) {
        while let Some((at, counts)) = self.cut.ripe(time) {
            self.counts(counts);
            if let Some(start) = self.cut.mark(at, self.builder.accounts()) {
                self.builder.forget_before(start.cast_signed());
                self.first = Some(start);
            }
        }
    }

    /// Processes the records in `batch` and those held back before, in
    /// time order, up to `watermark`; holds back the rest. Rings are
    /// drained one after another, so a record of one may be written after
    /// a later record of another was drained: the watermark lies far
    /// enough behind the drain that every record before it has arrived,
    /// and one that still comes later is counted.
    pub fn ingest(&mut self, batch: &[u8], watermark: Option<u64>) {
        let mut batches = std::mem::take(&mut self.batches);
        let blocks = batches.copy(batch);
        self.ingest_blocks(batches, blocks, watermark);
    }

    /// Ingests the blocks `drain` hands on, as [`Processor::ingest`] does a
    /// batch, giving it back the blocks emptied since. A busy recording
    /// drains tens of megabytes at a time, which this does not copy.
    pub fn ingest_drained(
        &mut self,
        drain: impl FnOnce(&mut perf::Blocks),
        watermark: Option<u64>,
    ) {
        let mut batches = std::mem::take(&mut self.batches);
        let blocks = batches.fill(drain);
        self.ingest_blocks(batches, blocks, watermark);
    }

    /// Ingests as [`Processor::ingest_drained`] does, up to `watermark`,
    /// but finds the instrumentation of every module mapped first, and
    /// stops at the mapping of a module whose debug information is not yet
    /// loaded, so that the collector reads that instrumentation before the
    /// load: a program waits for microscope at its exit only so long.
    /// [`Processor::load_stopped`] loads it, and the next drain goes on
    /// from the mapping.
    pub fn ingest_drained_after_probes(
        &mut self,
        drain: impl FnOnce(&mut perf::Blocks),
        watermark: u64,
    ) {
        self.loads = Loads::AfterProbes;
        self.ingest_drained(drain, Some(watermark));
        self.loads = Loads::AsMapped;
    }

    fn ingest_blocks(
        &mut self,
        mut batches: Batches,
        blocks: std::ops::Range<u64>,
        watermark: Option<u64>,
    ) {
        self.stopped = None;
        let outputs = self
            .interpreted
            .as_mut()
            .map(|interpreted| {
                interpreted.batch();
                interpreted.outputs.clone()
            })
            .unwrap_or_default();

        // Every record, by where it lies: those held back, in their order,
        // then the blocks', in theirs.
        let mut records = std::mem::take(&mut self.records);
        records.clear();
        records.append(&mut self.held);
        let held = records.len();
        let mut order = std::mem::take(&mut self.order);
        order.clear();

        // A record of Python frames goes with the next sample of its
        // thread and CPU in its ring (see `interpreted`).
        let mut unpaired: Unpaired = Vec::new();
        let mut pairs: Pairs = HashMap::default();
        // The mappings and execs, which a drain that stops at a module to
        // load reads ahead for instrumentation.
        let mut mappings = Vec::new();
        let (mut next_held, mut block, mut offset) = (0, blocks.start, 0);

        loop {
            let bytes = match records.get(next_held).filter(|_| next_held < held) {
                Some(&at) => batches.record(at),
                None if block < blocks.end => {
                    batches.bytes(block).get(offset..).unwrap_or_default()
                }
                None => break,
            };
            let next = decode::next_with_period(bytes, &outputs, self.fixed_period);
            let Some(Ok((record, length))) = next else {
                // A block that ends, or whose rest cannot be read, gives way
                // to the next, which another ring's drain began.
                if next.is_some() {
                    self.builder.count(AccountKind::Malformed as u32, 1);
                }
                if next_held < held {
                    next_held += 1;
                } else {
                    (block, offset) = (block + 1, 0);
                }
                continue;
            };
            let index = if next_held < held {
                next_held += 1;
                next_held - 1
            } else {
                records.push(RecordAt::new(block, offset, length));
                offset += length;
                records.len() - 1
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
                    self.output_drained(&mut unpaired, pid, (tid, cpu, time), data);
                    unpaired.push(((tid, cpu), time, index));
                    continue;
                }
                decode::Record::Sample(sample) => {
                    let key = (sample.tid, sample.cpu);
                    if let Some(at) = unpaired.iter().position(|(of, ..)| *of == key) {
                        let (_, time, pair) = unpaired.swap_remove(at);
                        pairs.insert(index, ((key.0, key.1, time), pair));
                    }
                }
                decode::Record::Mmap(mmap) => {
                    self.mapped_early(&mmap);
                    mappings.push((time, index));
                }
                decode::Record::Comm { exec: true, .. } => mappings.push((time, index)),
                _ => {}
            }

            order.push((time, index));
        }

        order.sort_unstable();
        if self.loads == Loads::AfterProbes {
            mappings.sort_unstable();
            self.find_probes_ahead(&mappings, &records, &batches, watermark);
        }
        let ordered = (order.as_slice(), records.as_slice(), &pairs);
        let watermark = self.process_ordered(ordered, &batches, &outputs, watermark);
        self.processed_to = self.processed_to.max(watermark.unwrap_or(0));

        self.hold_unpaired(unpaired, &records, watermark);
        self.recycle_paired();
        batches.release(&mut self.held);
        self.batches = batches;
        self.records = records;
        self.order = order;
        self.account_before(watermark.unwrap_or(u64::MAX));
        self.processed = watermark.unwrap_or(self.latest).max(self.processed);
        self.map_lost_before(watermark.unwrap_or(u64::MAX));
    }

    /// Processes the records of `order` up to `watermark`, holding back
    /// the rest, and returns the watermark processing reached: just before
    /// a mapping it stopped at, as [`Processor::ingest_drained_after_probes`]
    /// may, which it holds with every record after it.
    fn process_ordered(
        &mut self,
        (order, records, pairs): Ordered<'_>,
        batches: &Batches,
        outputs: &[u64],
        watermark: Option<u64>,
    ) -> Option<u64> {
        let record = |index| record_at(records, index);
        let mut stopped = None;
        for &(time, index) in order {
            let at = record(index);
            let pair = pairs.get(&index).map(|&(key, pair)| (key, record(pair)));
            if stopped.is_some() || watermark.is_some_and(|watermark| time > watermark) {
                // A held-back sample keeps its frames before it.
                self.held.extend(pair.map(|(_, pair)| pair));
                self.held.push(at);
                continue;
            }

            self.paired = pair.and_then(|(key, pair)| {
                let data = self.output_data(batches.record(pair), outputs)?;
                Some((key, data))
            });

            if let Some(Ok((record, _))) =
                decode::next_with_period(batches.record(at), outputs, self.fixed_period)
            {
                self.process(record);
            }

            if self.stopped.is_some() {
                self.held.extend(pair.map(|(_, pair)| pair));
                self.held.push(at);
                stopped = Some(time);
            }
        }

        let Some(time) = stopped else {
            return watermark;
        };
        let before = time.saturating_sub(1);
        Some(watermark.map_or(before, |watermark| watermark.min(before)))
    }

    /// Gives the interpreter's frames of thread `tid` on CPU `cpu`, which
    /// process `pid` put out at `time`, to the interpreter, which drops the
    /// thread's frames still `unpaired` there: they never found a sample.
    fn output_drained(
        &mut self,
        unpaired: &mut Unpaired,
        pid: Pid,
        (tid, cpu, time): (Tid, u32, u64),
        data: &[u8],
    ) {
        let (processes, images) = (&self.processes, &self.images);
        let Some(interpreted) = self.interpreted.as_mut() else {
            return;
        };
        if let Some(index) = unpaired.iter().position(|(of, ..)| *of == (tid, cpu)) {
            let (_, superseded, _) = unpaired.remove(index);
            interpreted.dropped((tid, cpu, superseded));
        }

        let code = |address| code_at(processes, images, pid, address);
        interpreted.drained(pid, (tid, cpu, time), data, &code);
    }

    /// The interpreter's frames the output record `bytes` carries, in the
    /// buffer kept for them.
    fn output_data(&mut self, bytes: &[u8], outputs: &[u64]) -> Option<Vec<u8>> {
        let Some(Ok((decode::Record::Output { data, .. }, _))) = decode::next_of(bytes, outputs)
        else {
            return None;
        };
        let mut kept = std::mem::take(&mut self.paired_bytes);
        kept.clear();
        kept.extend_from_slice(data);
        Some(kept)
    }

    /// Holds back each record of `unpaired` whose sample may still come;
    /// one whose sample the ring dropped goes with it.
    fn hold_unpaired(&mut self, unpaired: Unpaired, records: &[RecordAt], watermark: Option<u64>) {
        for ((tid, cpu), time, index) in unpaired {
            if watermark.is_some_and(|watermark| {
                time.saturating_add(interpreted::PAIRING_SLACK) > watermark
            }) {
                self.held.push(record_at(records, index));
            } else if let Some(interpreted) = &mut self.interpreted {
                interpreted.dropped((tid, cpu, time));
            }
        }
    }

    pub(super) fn process(&mut self, record: decode::Record<'_>) {
        let time = record.time().unwrap_or(self.latest);
        self.account_before(time);
        self.map_lost_before(time);
        // Late: older than a record processed already, or than a watermark
        // that said every record before it had come.
        if time < self.latest.max(self.processed_to) {
            self.builder.count(AccountKind::LateRecords as u32, 1);
        }

        self.latest = self.latest.max(time);
        // A capture holds nothing before its start. Before a cut, a late
        // record moves the start back to it; after one, the cut forgets it.
        if self.cut.start().is_none() {
            self.first = Some(self.first.map_or(time, |first| first.min(time)));
        }
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
                    state.execs += 1;
                    self.thread_changes += 1;
                    state.command_read = false;
                    state.mapped.edit().clear();
                    state.before_v8.clear();
                    state.jit.exec();
                    if let Some(interpreted) = &mut self.interpreted {
                        interpreted.forget(pid);
                        interpreted.forget_early(pid, time);
                    }

                    // Its instrumentation went with it; the new program's
                    // is found as it maps.
                    self.probes_executed_by(pid);
                    let process = self.builder.process(pid, at);
                    self.builder.process_name(process, &name);
                    self.builder.setting(&super::command_setting(process), "");
                    self.builder
                        .setting(&super::arguments_setting(process), "[]");
                } else if tid == pid.leader() {
                    let process = self.builder.process(pid, at);
                    self.builder.process_name(process, &name);
                }

                if exec || tid == pid.leader() {
                    self.read_command(pid, at);
                }
            }
            decode::Record::Fork {
                pid,
                ppid,
                tid,
                ptid,
                ..
            } => self.fork((pid, tid), (ppid, ptid), at),

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
                self.thread_changes += 1;
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

    /// Reads argv at lifecycle records, before a short-lived process may
    /// exit. The first sample refreshes it for programs that rewrite argv,
    /// as Chromium does to name each process's role.
    fn read_command(&mut self, pid: Pid, time: i64) {
        // A process that began after the record took its pid since.
        if !began_by(pid.get(), time) {
            return;
        }

        let Ok(bytes) = std::fs::read(format!("/proc/{}/cmdline", pid.get())) else {
            return;
        };
        let command = bytes
            .split(|byte| *byte == 0)
            .filter(|argument| !argument.is_empty())
            .map(String::from_utf8_lossy)
            .collect::<Vec<_>>()
            .join(" ");
        if !command.is_empty() {
            let process = self.builder.process(pid, time);
            self.builder
                .setting(&super::command_setting(process), &command);
            self.builder.setting(
                &super::arguments_setting(process),
                &super::encode_cmdline(&bytes),
            );
        }
    }

    /// Starts thread `tid` of process `pid`, which thread `maker` of
    /// process `parent` made.
    fn fork(&mut self, (pid, tid): (Pid, Tid), (parent, maker): (Pid, Tid), at: i64) {
        self.thread_changes += 1;
        if tid == pid.leader() && pid != parent {
            // A forked process starts with its parent's mappings.
            let (mapped, before_v8) = self
                .processes
                .get(&parent)
                .map(|parent| (parent.mapped.clone(), parent.before_v8.clone()))
                .unwrap_or_default();
            let child = self.processes.entry(pid).or_default();
            child.mapped = mapped;
            child.before_v8 = before_v8;
        }

        // The kernel writes a name only when a thread executes a program or
        // names itself; until then it has its maker's.
        let thread = self.thread(pid, tid, at);
        self.builder.inherit_name(thread, maker);
        if tid == pid.leader() && pid != parent {
            self.read_command(pid, at);
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
                let defer = self.loads == Loads::AfterProbes;
                (self.image(path, &file, mmap.inode, defer), bias)
            })
        };

        // A module whose debug information is still to load is mapped once
        // it has. Its instrumentation is found now, so that the collector
        // reads it before the load, and processing stops at this record.
        if let Some((image, bias)) = image {
            self.found_probes_in(mmap, image, bias);
            if image_of(&self.images, image).pending.is_some() {
                self.stopped = Some(image);
                return;
            }
        }

        let state = self.processes.entry(mmap.pid).or_default();
        let images = &self.images;
        let replaces_module = state.mapped.iter().any(|mapped| {
            let image = image_of(images, mapped.image);
            mapped.start < end && mmap.start < mapped.end && !image.anonymous && !image.generated
        });

        // A mapping replaces whatever it overlaps.
        uncover(state.mapped.edit(), mmap.start, end);
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
        if image.is_some() || replaces_module {
            state.mappings += 1;
            self.thread_changes += 1;
        }
        state.jit.mapped(
            mmap.start,
            end,
            perf::jit::is_anonymous_code(mmap),
            mmap.time,
        );

        let Some((image, bias)) = image else {
            if perf::jit::is_anonymous_code(mmap) {
                let carries_v8 = state
                    .mapped
                    .iter()
                    .any(|mapped| image_of(&self.images, mapped.image).v8);
                if carries_v8 {
                    self.v8_code(mmap.pid, mmap.time, mmap.start..end);
                } else {
                    state.before_v8.push(mmap.start..end);
                }
            }
            return;
        };

        let loaded = LoadedModule {
            id: ModuleId::new(self.next_module),
            image: image_of(&self.images, image).id,
            load_bias: bias,
            window: None,
        };

        self.next_module += 1;

        state.mapped.edit().push(Mapped {
            start: mmap.start,
            end,
            image,
            loaded,
        });
        self.builder.mapping(Mapping {
            process: process.into(),
            module: image_of(&self.images, image).module.into(),
            start: mmap.start.into(),
            end: end.into(),
            file_offset: mmap.file_offset.into(),
            bias: bias.get().into(),
            time: mmap.time.cast_signed().into(),
        });

        let state = self.processes.entry(mmap.pid).or_default();
        let is_v8 = image_of(&self.images, image).v8;
        // V8's code is unwound by its convention, and its perf map read
        // only for names (see `perf_map_module`).
        state.jit.names_only |= is_v8;
        let carries_v8 = !state.before_v8.is_empty() && is_v8;
        if carries_v8 {
            for code in std::mem::take(&mut state.before_v8) {
                self.v8_code(mmap.pid, mmap.time, code);
            }
        }
    }

    /// Gives the collector the instrumentation of image `image`, which
    /// `mmap` maps at `bias`, the first time the process maps it.
    fn found_probes_in(&mut self, mmap: &decode::Mmap<'_>, image: usize, bias: LoadBias) {
        if let Some(table) = &image_of(&self.images, image).probes
            && self.probes_mapped.insert((mmap.pid, image))
        {
            let found = Found {
                module: image_of(&self.images, image).module,
                path: String::from_utf8_lossy(mmap.path).into(),
                bias,
                table: Arc::clone(table),
            };

            self.probes_found.push((mmap.pid, found));
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

    /// The image of the file at `path`, read the first time. Its debug
    /// information is loaded then too, unless `defer`, which leaves it to
    /// [`Processor::load_stopped`] or the mapping's processing.
    fn image(
        &mut self,
        path: PathBuf,
        file: &std::path::Path,
        inode: Option<u64>,
        defer: bool,
    ) -> usize {
        let key = (path, inode.unwrap_or(0));
        let image = if let Some(&image) = self.image_index.get(&key) {
            image
        } else {
            let _span = crate::span!("record image", "{}", file.display());
            let data = crate::image::backing::read_input(file)
                .ok()
                .map(|(data, _)| data);
            let image = self.register_image(&key.0, data);
            self.image_index.insert(key, image);
            image
        };

        if !defer {
            self.complete_image(image);
        }
        image
    }

    /// Adds the image of a module whose file holds `data`, if it could be
    /// read, with its debug information.
    fn add_image(&mut self, path: &std::path::Path, data: Option<Vec<u8>>) -> usize {
        let image = self.register_image(path, data);
        self.complete_image(image);
        image
    }

    /// Whether the last drain stopped at a module still to load, so that
    /// the next goes on from it.
    pub const fn stopped_at_load(&self) -> bool {
        self.stopped.is_some()
    }

    /// Whether an image's debug information is still to be loaded.
    #[cfg(test)]
    fn images_pending(&self) -> bool {
        self.images.iter().any(|image| image.pending.is_some())
    }

    /// Finds the instrumentation of every module the `mappings` up to
    /// `watermark` map, in time order with the execs among them, before
    /// any of their debug information loads, so that the collector reads it
    /// first. What a process maps after it executes a program is the next
    /// program's, which the collector takes only once it has seen the exec.
    fn find_probes_ahead(
        &mut self,
        mappings: &[(u64, usize)],
        records: &[RecordAt],
        batches: &Batches,
        watermark: Option<u64>,
    ) {
        let mut executed = BTreeSet::new();
        for &(time, index) in mappings {
            if watermark.is_some_and(|watermark| time > watermark) {
                break;
            }

            match decode::next(batches.record(record_at(records, index))) {
                Some(Ok((
                    decode::Record::Comm {
                        pid, exec: true, ..
                    },
                    _,
                ))) => {
                    executed.insert(pid);
                }
                Some(Ok((decode::Record::Mmap(mmap), _)))
                    if !executed.contains(&mmap.pid) && !perf::is_vdso(&mmap) =>
                {
                    if let Some((file, bias)) = perf::identify(&mmap) {
                        let path = PathBuf::from(String::from_utf8_lossy(mmap.path).into_owned());
                        let image = self.image(path, &file, mmap.inode, true);
                        self.found_probes_in(&mmap, image, bias);
                    }
                }
                _ => {}
            }
        }
    }

    /// Loads the debug information of the module whose mapping the last
    /// drain stopped at, once the collector has read the instrumentation
    /// the drain found. One at a time, so that a module the program maps
    /// meanwhile, which may hold its instrumentation, waits for one load
    /// rather than for every module mapped before it.
    pub fn load_stopped(&mut self) {
        if let Some(image) = self.stopped {
            self.complete_image(image);
        }
    }

    /// Adds the image of a module whose file holds `data`, as far as its
    /// file says without its debug information: its identity and its
    /// instrumentation, which a recording may read before the rest loads.
    fn register_image(&mut self, path: &std::path::Path, data: Option<Vec<u8>>) -> usize {
        let began = self.own_cpu();
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
            v8: false,
            info: None,
            id,
            module,
            probes,
            anonymous: false,
            deferred: None,
            generated: false,
            code_keepers: std::sync::OnceLock::new(),
            copied: None,
            pending: Some(Pending {
                path: path.to_owned(),
                data,
                digest,
                build_id,
            }),
        });
        self.loading += self.own_cpu().saturating_sub(began);
        image
    }

    /// Loads the debug information of image `image`, if it has not been.
    fn complete_image(&mut self, image: usize) {
        let Some(Pending {
            path,
            data,
            digest,
            build_id,
        }) = image_of_mut(&mut self.images, image).pending.take()
        else {
            return;
        };

        let began = self.own_cpu();
        let id = image_of(&self.images, image).id;
        #[cfg(debug_assertions)]
        hold_load(&path);
        let loaded = data.and_then(|data| {
            if self.debug_loading == DebugLoading::AfterCollection && path.is_absolute() {
                crate::debug_info::load_profile_module_bytes(&path, &data, id, &self.search).ok()
            } else {
                crate::debug_info::load_module_bytes(&path, &data, id, &self.search)
                    .ok()
                    .map(|info| (info, false))
            }
        });
        let deferred = loaded.as_ref().is_some_and(|(_, deferred)| *deferred);
        let info = loaded.map(|(info, _)| info);

        // An image the cache could not keep is read again by every later
        // recording, which a large program's recordings notice.
        for (unwritten, why) in crate::cache::take_unwritten() {
            self.builder
                .setting(&format!("image cache {}", unwritten.display()), &why);
        }

        // What the debug information leaves out costs a profile its
        // inlines and names, as split DWARF whose `.dwo` files are gone
        // does; leaving out locals costs it nothing.
        if let Some(info) = &info
            && !deferred
            && let crate::DebugInformation::Incomplete { reason } = info.image.debug_information()
            && info.image.locals_left_out().as_deref() != Some(&*reason)
        {
            self.builder.setting(
                &format!("debug information {}", path.display()),
                &format!("incomplete: {reason}"),
            );
        }

        let entry = image_of_mut(&mut self.images, image);
        entry.v8 = info
            .as_ref()
            .is_some_and(|info| crate::debug_info::v8::is_v8(&info.image));
        entry.info = info;
        entry.deferred = deferred.then_some((digest, build_id));
        let spent = self.own_cpu().saturating_sub(began);
        self.loading += spent;
        record!(
            "record: loaded {} in {} ms of CPU",
            path.display(),
            spent.as_millis()
        );
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
        let carries_v8 = held
            .iter()
            .any(|mapped| image_of(&self.images, mapped.image).v8);
        if carries_v8 {
            let generated: Vec<std::ops::Range<u64>> = held
                .iter()
                .filter(|mapped| {
                    image_of(&self.images, mapped.image).generated
                        && mapped.start < stale.end
                        && stale.start < mapped.end
                })
                .map(|mapped| mapped.start.max(stale.start)..mapped.end.min(stale.end))
                .collect();
            for code in generated {
                self.generated_module(pid, time, stale.range, code, &stale.functions);
                // A module is found by its image's bounds, and those of the
                // module it replaces may span more than its pieces.
                let held = self.processes.entry(pid).or_default().mapped.edit();
                if let Some(named) = held.pop() {
                    held.insert(0, named);
                }
            }
            return;
        }

        let began = self.own_cpu();
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
        self.loading += self.own_cpu().saturating_sub(began);
    }

    /// Makes the module of an object a JIT compiler registered, mapped at
    /// the addresses its code runs at, in place of what they held.
    fn jit_object_module(&mut self, pid: Pid, time: u64, object: &JitCode) {
        let began = self.own_cpu();
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
        self.loading += self.own_cpu().saturating_sub(began);
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
        // A copy of the builtins binds V8's image too, but V8 copies the
        // module itself. Each worker's isolate adds a copy, so reading the
        // copies as sources made each new isolate cost one more than the
        // last.
        let v8: Vec<(usize, &Mapped, &Image, &DebugInfo)> = process
            .mapped
            .iter()
            .enumerate()
            .filter(|(_, mapped)| mapped.loaded.window.is_none())
            .filter_map(|(index, mapped)| {
                let image = image_of(&self.images, mapped.image);
                let info = image.info.as_ref().filter(|_| image.v8)?;
                Some((index, mapped, image, info))
            })
            .collect();
        if v8.is_empty() {
            return;
        }

        let read_only = perf::mapping_at(pid.get(), code.start).is_ok_and(|mapping| {
            mapping.is_some_and(|mapping| code.end <= mapping.end && !mapping.writable)
        });
        let copy = read_only
            .then(|| {
                let sources: Vec<crate::unwind::code_copies::Source<'_, usize>> = v8
                    .iter()
                    .map(
                        |&(index, mapped, image, info)| crate::unwind::code_copies::Source {
                            key: index,
                            image: &info.image,
                            loaded: mapped.loaded,
                            stored: image
                                .copied
                                .into_iter()
                                .chain(perf::stored_code_of(
                                    image.code_keepers.get_or_init(|| perf::code_keepers(info)),
                                    pid.get(),
                                    mapped.loaded,
                                    mapped.start..mapped.end,
                                ))
                                .collect(),
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
            self.generated_module(pid, time, code.start, code, &[]);
            return;
        };

        image_of_mut(&mut self.images, source.image).copied = Some((
            image_start,
            ImageAddress::new(image_start.get().saturating_add(length)),
        ));
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
        let module = image_of(&self.images, source.image).module;
        let process = self.builder.process(pid, time.cast_signed());
        let held = self.processes.entry(pid).or_default().mapped.edit();
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
    /// names there for its symbols. The module is named for `mapped`, where
    /// the mapping that holds it begins, so that each chunk of one mapping,
    /// and each module of it made again, has the same path.
    fn generated_module(
        &mut self,
        pid: Pid,
        time: u64,
        mapped: u64,
        code: std::ops::Range<u64>,
        jitted: &[crate::debug_info::perf_map::Jitted],
    ) {
        let began = self.own_cpu();
        let id = ModuleImageId::new(u32::try_from(self.images.len()).unwrap_or(u32::MAX));
        let path = format!("[V8 code {mapped:#x}]");
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
        self.loading += self.own_cpu().saturating_sub(began);
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
            v8: info
                .as_ref()
                .is_some_and(|info| crate::debug_info::v8::is_v8(&info.image)),
            info,
            id,
            module,
            probes: None,
            anonymous: true,
            deferred: None,
            generated: false,
            code_keepers: std::sync::OnceLock::new(),
            copied: None,
            pending: None,
        });

        let loaded = LoadedModule {
            id: ModuleId::new(self.next_module),
            image: id,
            load_bias: LoadBias::new(0),
            window: None,
        };
        self.next_module += 1;
        let process = self.builder.process(pid, time.cast_signed());
        let held = self.processes.entry(pid).or_default().mapped.edit();
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

    /// Unwinds a sample through the modules of its process into `frames`,
    /// with the modules its frames index.
    fn unwind(
        &mut self,
        sample: &decode::Sample<'_>,
        frames: &mut Vec<SampledFrame>,
    ) -> (StackEnd, Arc<ModuleIndex>) {
        let modules = self
            .processes
            .entry(sample.pid)
            .or_default()
            .mapped
            .index(&self.images);
        let end = unwind_sample_into(
            &*modules,
            &mut self.rules,
            SampledState {
                registers: &self.registers,
                stack: sample.stack,
                chain: &self.chain,
            },
            self.limit,
            frames,
        );
        (end, modules)
    }

    /// Makes again the modules of anonymous code a sample's frames or
    /// chain landed in that the perf map of its process may name since:
    /// whether any changed.
    fn refresh_perf_map(
        &mut self,
        sample: &decode::Sample<'_>,
        frames: &[SampledFrame],
        addresses: &mut Vec<u64>,
    ) -> bool {
        addresses.clear();
        addresses.extend(
            frames
                .iter()
                .map(|frame| frame.address.get())
                .chain(self.chain.first().copied())
                .chain(
                    self.chain
                        .iter()
                        .skip(1)
                        .map(|address| address.wrapping_sub(1)),
                ),
        );
        let addresses = addresses.as_slice();
        let recovered = self.map_unseen_code(sample, addresses);

        let Some(process) = self.processes.get_mut(&sample.pid) else {
            return recovered;
        };

        let images = &self.images;
        let mapped = &process.mapped;
        let descriptors = || {
            let descriptors: BTreeSet<u64> = mapped
                .iter()
                .filter_map(|mapped| Some((mapped, image_of(images, mapped.image).info.as_ref()?)))
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

        // Code V8 generated is anonymous code too, which the map may name,
        // and so is a module the map made: the map may name code there
        // that the program made after the module was made.
        let unnamed = addresses.iter().copied().filter(|&address| {
            mapped.piece_at(address).is_none_or(|mapped| {
                let image = image_of(images, mapped.image);
                (image.generated || image.anonymous)
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
    /// run it (see [`Processor::map_unseen_code`]), and to the live reads
    /// that find it, whose code is mapped here too.
    fn map_lost_before(&mut self, time: u64) {
        while self
            .live_unmapped
            .front()
            .is_some_and(|(read, ..)| *read <= time)
        {
            if let Some((read, pid, addresses)) = self.live_unmapped.pop_front() {
                self.map_unseen_code_of(pid, read, &addresses);
            }
        }

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
        self.map_unseen_code_of(sample.pid, sample.time, addresses)
    }

    /// Maps the anonymous code that holds the frames a live read of
    /// process `pid` that ended at `time` found in no mapping, as
    /// [`Self::map_unseen_code`] maps a sample's: a thread blocked under
    /// code whose mapping record was lost is never sampled there, so only
    /// its live reads find it. Records stamped before the read may not be
    /// processed yet, so the code is mapped once they are, as a lost
    /// mapping is ([`Self::map_lost_before`]).
    pub fn map_live_code(&mut self, pid: Pid, time: u64, addresses: &[u64]) {
        if !addresses.is_empty() {
            self.live_unmapped
                .push_back((time, pid, addresses.to_vec()));
        }
    }

    fn map_unseen_code_of(&mut self, pid: Pid, time: u64, addresses: &[u64]) -> bool {
        let Some(process) = self.processes.get(&pid) else {
            return false;
        };

        let unseen: Vec<u64> = addresses
            .iter()
            .copied()
            .filter(|&address| {
                process.mapped.piece_at(address).is_none() && process.jit.unseen(address, time)
            })
            .collect();
        if unseen.is_empty() {
            return false;
        }

        let mut mappings: Vec<perf::Mapped> = Vec::new();
        for &address in &unseen {
            if mappings
                .iter()
                .any(|mapping| (mapping.start..mapping.end).contains(&address))
            {
                continue;
            }
            match perf::mapping_at(pid.get(), address) {
                Ok(Some(mapping)) => mappings.push(mapping),
                Ok(None) => {}
                Err(_) => return false,
            }
        }

        mappings.sort_by_key(|mapping| mapping.start);

        let mut mapped = false;
        for mapping in &mappings {
            let mmap = decode::Mmap {
                pid,
                start: mapping.start,
                length: mapping.end - mapping.start,
                file_offset: mapping.file_offset,
                inode: Some(mapping.inode),
                executable: mapping.executable,
                path: &mapping.path,
                time,
            };
            if perf::jit::is_anonymous_code(&mmap) {
                self.mmap(&mmap);
                mapped = true;
            }
        }

        mapped
    }

    fn kernel_leaf(&mut self, sample: &decode::Sample<'_>) -> Option<u32> {
        sample.kernel.then(|| {
            let call = self
                .paired
                .as_ref()
                .and_then(|(_, bytes)| crate::backend::bpf::sampler::kernel_syscall(bytes));
            let function = self.kernel_leaves.entry(call).or_insert_with(|| {
                call.map_or_else(
                    || "[kernel: unknown]".to_owned(),
                    |call| format!("[kernel: {}]", call.display_name()),
                )
            });
            let function = function.as_str();
            let name = self.builder.string(function);
            let leaf = self
                .builder
                .frame(FrameKind::Kernel, NONE, u64::from(name), 0);
            if !self.builder.symbolized(leaf) {
                self.builder.symbolize(
                    leaf,
                    NONE,
                    &[LineInput {
                        function,
                        file: None,
                        line: 0,
                        column: 0,
                        declared: None,
                        role: 0,
                    }],
                );
            }
            leaf
        })
    }

    fn sample(&mut self, sample: &decode::Sample<'_>) {
        let kernel = self.kernel_leaf(sample);
        let time = sample.time.cast_signed();
        let thread = self.thread(sample.pid, sample.tid, time);
        let state = self.processes.entry(sample.pid).or_default();
        if !std::mem::replace(&mut state.command_read, true) {
            self.read_command(sample.pid, time);
        }
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

        let mut scratch = std::mem::take(&mut self.scratch);
        let (mut end, mut modules) = self.unwind(sample, &mut scratch.frames);
        if self.refresh_perf_map(sample, &scratch.frames, &mut scratch.addresses) {
            (end, modules) = self.unwind(sample, &mut scratch.frames);
        }
        let (frames, owners) = (&scratch.frames, modules.owners());

        if let Some(leaf) = frames.first() {
            self.note_task(sample.pid, thread, time, leaf, owners);
        }

        self.native_frames(frames, owners, (&mut scratch.named, &mut scratch.keys));
        let (interpreted, running) = self.sampled_interpreted(sample, thread);
        self.activations(sample.pid, &scratch.named, &mut scratch.native);
        let native = &scratch.native;

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
            self.splice_placed(native, (frames, &scratch.keys), &placed);
            cut
        } else {
            self.splice_interpreted(native, interpreted).or(cut)
        };

        let truncation = match end {
            StackEnd::Truncated(reason, module) => Some((
                reason,
                module
                    .and_then(|module| owners.get(module))
                    .map_or(NONE, |owner| image_of(&self.images, owner.image).module),
            )),
            StackEnd::Complete => marker.map(|marker| (marker, NONE)),
        };
        self.scratch = scratch;

        if let Some((reason, module)) = truncation {
            let frame = self.builder.truncation(reason, module);
            self.frames.push(frame);
            self.builder
                .count(AccountKind::Truncated as u32 + reason as u32, 1);
        }

        if let Some(leaf) = kernel {
            self.frames.insert(0, leaf);
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
        let sampled = interpreted.paired(
            sample.pid,
            paired.as_ref().map(|(key, bytes)| (*key, bytes.as_slice())),
            &code,
        );
        let running = stack.and_then(|stack| stack.running_task);
        self.paired = paired;
        self.recycle_paired();
        (sampled, running)
    }

    /// Keeps the buffer of the record paired with the sample just
    /// processed, for the next one's.
    fn recycle_paired(&mut self) {
        if let Some((_, bytes)) = self.paired.take() {
            self.paired_bytes = bytes;
        }
    }

    /// Puts in `native` each of process `pid`'s `named` frames, with
    /// whether it is an activation of an interpreter's loop.
    fn activations(&self, pid: Pid, named: &[NamedFrame], native: &mut Vec<(u32, bool)>) {
        native.clear();
        let inners = std::iter::once(None).chain(named.iter().map(|(_, name)| name.as_deref()));
        native.extend(named.iter().zip(inners).map(|((frame, name), inner)| {
            let activation = name.as_deref().is_some_and(|name| {
                self.interpreted
                    .as_ref()
                    .is_some_and(|found| found.activation(pid, name, inner))
            });
            (*frame, activation)
        }));
    }

    /// Puts in `native` each of a sample's native `frames`, of the modules
    /// `owners`, as a frame of the capture with the name of the
    /// interpreter's code it runs, if it runs the interpreter's; and in
    /// `keys` each frame's module and address, its key.
    fn native_frames(
        &mut self,
        frames: &[SampledFrame],
        owners: &[Mapped],
        (native, keys): (&mut Vec<NamedFrame>, &mut Vec<(u32, u64)>),
    ) {
        native.clear();
        keys.clear();
        for frame in frames {
            let owner = frame
                .module
                .and_then(|module| owners.get(module).copied())
                .filter(|owner| self.names(owner, frame.address.get()));
            let key = owner.map_or_else(
                || (NONE, frame.address.get()),
                |owner| {
                    (
                        image_of(&self.images, owner.image).module,
                        owner.loaded.load_bias.wrapping_unload(frame.address).get(),
                    )
                },
            );
            // Code V8 generated runs only its functions and its own
            // machinery, as an interpreter's loop does: a sample's frames
            // of V8's name its functions where it has them.
            let generated = frame
                .module
                .and_then(|module| owners.get(module))
                .is_some_and(|owner| image_of(&self.images, owner.image).generated);
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
                    let info = image_of(&self.images, owner.image).info.as_ref()?;
                    let address = owner.loaded.load_bias.wrapping_unload(frame.address);
                    Some(info.image.symbolize(address)?.name)
                });
            native.push((id, name));
        }
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
            let id = self.builder.interpreted(&wait.place());
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
                let id = self.builder.interpreted(&wait.place());
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
        let _span = crate::span!("record snapshot");
        self.symbolize_new();
        let mut builder = self.builder.clone();
        if self.images.iter().any(|image| image.deferred.is_some()) {
            builder.setting("symbols", "later");
        }
        let end = self.end(&mut builder, self.latest);
        self.close(&mut builder, end);
        self.start(&mut builder);
        builder.finish()
    }

    /// The finished capture, every frame symbolized.
    pub fn finish(mut self) -> Capture {
        let _span = crate::span!("record finish");
        // Everything held back is processed now.
        self.ingest(&[], None);
        if !self.symbols_later {
            self.enrich();
        }
        self.symbolize_new();
        let mut builder = std::mem::take(&mut self.builder);
        let end = self.end(&mut builder, self.latest);
        self.close(&mut builder, end);
        self.start(&mut builder);
        builder.finish()
    }

    /// What a save writes: the capture so far, as [`Processor::snapshot`]
    /// makes it, or in a segmented recording, the segment since the last,
    /// or since the recording began, up to its latest counted counts,
    /// which begins the next. A segment is a complete capture of that span
    /// alone, every frame symbolized. What spans the cut, a thread off its
    /// CPU or an open zone, is in each segment for the part of it there.
    pub fn save(&mut self) -> Capture {
        if !matches!(self.cut, Cut::Segments { .. }) {
            return self.snapshot();
        }

        self.symbolize_new();
        let mut builder = self.builder.clone();
        let end = self.end(&mut builder, self.processed);
        self.close(&mut builder, end);
        let accounts = builder.accounts().clone();
        self.start(&mut builder);
        builder.forget_from(end.cast_signed());
        builder.setting("end", &end.to_string());

        self.builder.forget_before(end.cast_signed());
        if let Cut::Segments { start, .. } = &mut self.cut {
            *start = Some(cuts::Mark {
                time: end,
                accounts,
            });
        }
        self.first = Some(end);
        builder.finish()
    }

    /// Ends `builder` where the cut does, with the counts counted there,
    /// or a whole recording at `fallback`.
    fn end(&self, builder: &mut CaptureBuilder, fallback: u64) -> u64 {
        match self.cut.end() {
            End::Records => fallback,
            End::Unobserved => {
                // Until the first counts read are counted, no span is.
                let end = self.first.unwrap_or(fallback);
                builder.restore_accounts(&BTreeMap::new());
                builder.setting("start", &end.to_string());
                builder.setting("end", &end.to_string());
                end
            }
            End::At(mark) => {
                builder.restore_accounts(&mark.accounts);
                builder.setting("end", &mark.time.to_string());
                mark.time
            }
        }
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
                self.cpu().saturating_sub(self.began),
            ),
            (AccountKind::RecorderLoading, self.loading),
            (AccountKind::RecorderSetup, self.setup.unwrap_or_default()),
        ] {
            let charged = builder.account(kind as u32);
            builder.count(kind as u32, nanoseconds(total).saturating_sub(charged));
        }

        if !matches!(self.cut, Cut::Whole) {
            builder.forget_from(end.cast_signed());
        }
    }

    /// Cuts a window's rows and counts, or a segment's, to its start.
    fn start(&self, builder: &mut CaptureBuilder) {
        if let Some(start) = self.cut.start() {
            builder.forget_before(start.time.cast_signed());
            builder.discount(&start.accounts);
            builder.setting("start", &start.time.to_string());
        }
    }

    /// Forgets the instrumentation the program process `pid` replaced
    /// held, keeping what the collector never took to say it went unread.
    fn probes_executed_by(&mut self, pid: Pid) {
        self.probes_mapped.retain(|(process, _)| *process != pid);
        let (gone, kept) = std::mem::take(&mut self.probes_found)
            .into_iter()
            .partition(|(process, _)| *process == pid);
        self.probes_found = kept;
        self.probes_executed
            .entry(pid)
            .or_default()
            .extend(gone.into_iter().map(|(_, found)| found));
    }

    /// What process `pid` has mapped and executed since last asked.
    pub fn found_probes(&mut self, pid: Pid) -> FoundProbes {
        let skipped = self.probes_executed.remove(&pid);
        let (found, others) = std::mem::take(&mut self.probes_found)
            .into_iter()
            .partition(|(process, _)| *process == pid);
        self.probes_found = others;

        FoundProbes {
            executed: skipped.is_some(),
            skipped: skipped.unwrap_or_default(),
            found: found.into_iter().map(|(_, found)| found).collect(),
        }
    }

    /// When process `pid` exited, on the recording's clock, if it has.
    pub fn exited_at(&self, pid: Pid) -> Option<u64> {
        self.builder
            .process_end(pid)
            .and_then(|end| u64::try_from(end).ok())
    }

    /// Says that a module's instrumentation was not read to its end.
    pub fn probes_unread(&mut self, text: &str) {
        self.builder.count(AccountKind::ProbesUnread as u32, 1);
        self.probe_problem(text);
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

    /// The row of a thread at the time its instrumentation was recorded.
    pub fn probe_thread(&mut self, pid: Pid, tid: Tid, time: i64) -> u32 {
        self.thread(pid, tid, time)
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
            interpreted.forget_early(pid, u64::MAX);
        }

        if let Some(process) = self.processes.get(&pid) {
            for tid in &process.threads {
                self.switched_out.remove(tid);
            }
        }
    }

    /// Records that sampling was off from `start` to `end`, and why. A
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

        debug_assert!(first <= last, "gaps {first}..{last} for {start}..{end}");
        #[expect(
            clippy::indexing_slicing,
            reason = "the gaps are sorted and apart, so first <= last <= len"
        )]
        let covering = &self.gaps[first..last];
        for &(covered, until) in covering {
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

        let mut merged = covering.to_vec();
        merged.extend_from_slice(&pieces);
        merged.sort_unstable();
        self.gaps.splice(first..last, merged);
        self.gaps_end = self.gaps_end.max(end);
        debug_assert!(
            self.gaps
                .windows(2)
                .all(|pair| matches!(pair, [a, b] if a.1 <= b.0)),
            "the gaps overlap after adding {start}..{end}"
        );
    }

    /// Records that the budget turned sampling off at `start`, a pause that
    /// becomes a gap only when it ends. Records stamped in it are processed
    /// meanwhile, and a thread off its CPU before it is cut where it began.
    pub fn pause(&mut self, start: u64) {
        debug_assert!(self.paused.is_none(), "a pause is already open at {start}");
        self.paused = Some(start);
    }

    /// Ends the open budget pause at `end`, as a gap.
    pub fn resume(&mut self, end: u64) {
        debug_assert!(self.paused.is_some(), "no pause is open to end at {end}");
        if let Some(start) = self.paused.take() {
            self.gap(start, end, GapReason::Budget);
        }
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

        let Some(owner) = leaf.module.and_then(|module| owners.get(module)) else {
            return;
        };

        let address = owner.loaded.load_bias.wrapping_unload(leaf.address);
        let register = image_of(&self.images, owner.image)
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
        let read = u64::try_from(snapshot.waiting.len()).unwrap_or(u64::MAX);
        self.builder.count(
            AccountKind::LiveRead as u32,
            read.saturating_add(snapshot.changed),
        );
        if !snapshot.changed_threads.is_empty() {
            let threads = snapshot
                .changed_threads
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            self.snapshot_noted(start, &format!("threads changed while read: {threads}"));
        }
        self.tasks(pid, (start, end), weight, snapshot);
    }

    /// Records a snapshot's tasks, notes, and problems.
    fn tasks(&mut self, pid: Pid, (start, end): (u64, u64), weight: u64, snapshot: &Snapshot) {
        self.builder.count(AccountKind::Snapshots as u32, 1);
        for note in &snapshot.notes {
            self.snapshot_noted(start, note);
        }

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
                    WaitFrame::Interpreted(frame) => self.builder.interpreted(&frame.place()),
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

    /// Each piece of process `pid`'s code a live read taken now finds code
    /// by, once the records stamped before `watermark()`, the watermark a
    /// drain begun now would take, are processed. A runtime runs the code
    /// it maps at once, so the read must find every mapping recorded before
    /// it.
    ///
    /// The records `drain` gives are processed only while those processed
    /// lag the watermark by more than [`READ_SLACK`], as the drain before
    /// the read leaves them if it ran long, for one that loads a module's
    /// debug information, and as a recording that reads more often than it
    /// drains does. Each drain that runs long is followed by another, up to
    /// [`READ_DRAINS`]: one that loads debug information leaves what the
    /// program mapped meanwhile past its watermark, and a recording that
    /// drains slowly throughout still reads.
    pub fn code_for_read(
        &mut self,
        pid: Pid,
        watermark: impl Fn() -> u64,
        mut drain: impl FnMut(&mut perf::Blocks),
    ) -> Vec<super::tasks::MappedCode> {
        for _ in 0..READ_DRAINS {
            // Taken before the drain, as the recording loop takes its own:
            // a drain that runs long must not pass records still to come
            // from a ring it has already read.
            let watermark = watermark();
            if self.processed_to.saturating_add(READ_SLACK) >= watermark {
                break;
            }
            self.ingest_drained(&mut drain, Some(watermark));
        }
        self.mapped_code(pid)
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

    /// Records why the task snapshot at `time`, which did not fail, ran
    /// otherwise than asked, as later than due.
    pub fn snapshot_noted(&mut self, time: u64, why: &str) {
        record!("record: {why}");
        self.builder
            .marker(time.cast_signed(), MarkerKind::TaskSnapshot, why);
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
                image_of(&self.images, owner.image).module,
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
        let image = image_of(&self.images, owner.image);
        !image.anonymous
            || image
                .info
                .as_ref()
                .is_some_and(|info| info.image.symbolize(ImageAddress::new(address)).is_some())
    }

    /// Where the first gap or open pause after `time` began, or the end of
    /// time.
    fn next_gap(&self, time: u64) -> u64 {
        let next = self.gaps.partition_point(|&(start, _)| start <= time);
        let gap = self.gaps.get(next).map_or(u64::MAX, |&(start, _)| start);
        self.paused
            .filter(|&paused| paused > time)
            .map_or(gap, |paused| gap.min(paused))
    }

    /// Ends the recorder's setup: what it costs from here on is the cost of
    /// recording the program.
    pub fn set_up(&mut self) {
        let spent = self.cpu().saturating_sub(self.began);
        self.setup.get_or_insert(spent);
    }

    /// The CPU time loading images has taken.
    pub const fn loading(&self) -> std::time::Duration {
        self.loading
    }

    /// Updates the additional CPU time used by the ring reader.
    pub const fn collection_cpu(&mut self, total: std::time::Duration) {
        self.collection = total;
    }

    /// The processor and reader's CPU time, and what the loader's workers
    /// have spent since the processor began, for the recording budget.
    pub fn cpu(&self) -> std::time::Duration {
        self.own_cpu() + self.collection
    }

    /// The processor's own CPU time, and the loader's workers' since it
    /// began: what loading an image costs, on whichever threads.
    fn own_cpu(&self) -> std::time::Duration {
        thread_cpu() + crate::pool::cpu().saturating_sub(self.pool_began)
    }

    /// Leaves every frame without lines, for a viewer to symbolize from
    /// its module's build id and address.
    pub fn symbols_later(&mut self) {
        self.symbols_later = true;
        self.builder.setting("symbols", "later");
    }

    /// Keeps full metadata off ordinary native collection's critical path.
    pub const fn defer_debug_information(&mut self) {
        self.debug_loading = DebugLoading::AfterCollection;
    }

    fn enrich(&mut self) {
        if !self.images.iter().any(|image| image.deferred.is_some()) {
            return;
        }
        let _span = crate::span!("record enrich");
        let began = self.own_cpu();
        let sampled: BTreeSet<u32> = self
            .builder
            .frames()
            .iter()
            .filter(|frame| frame.kind == FrameKind::Native as u8)
            .map(|frame| frame.module.get())
            .collect();
        let mut forget_indexes = false;
        for image in &mut self.images {
            if !sampled.contains(&image.module) {
                continue;
            }
            let Some((digest, build_id)) = image.deferred.take() else {
                continue;
            };
            let Some(info) = &image.info else { continue };
            let path = info.image.path().to_string_lossy().into_owned();
            match super::symbols::locate(&path, build_id.as_deref(), digest, image.id, &self.search)
            {
                Ok((_, info)) => {
                    if let crate::DebugInformation::Incomplete { reason } =
                        info.image.debug_information()
                        && info.image.locals_left_out().as_deref() != Some(&*reason)
                    {
                        self.builder.setting(
                            &format!("debug information {path}"),
                            &format!("incomplete: {reason}"),
                        );
                    }
                    image.info = Some(info);
                    forget_indexes = true;
                }
                Err(why) => self.builder.setting(
                    &format!("debug information {path}"),
                    &format!("final metadata unavailable: {why}; keeping collection symbols"),
                ),
            }
        }
        if forget_indexes {
            for process in self.processes.values_mut() {
                process.mapped.forget_index();
            }
        }
        let used = self.own_cpu().saturating_sub(began);
        self.loading += used;
        self.builder.setting(
            "final debug enrichment cpu ns",
            &used.as_nanos().to_string(),
        );
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
            .filter(|(_, image)| {
                image.anonymous || (!self.symbols_later && image.deferred.is_none())
            })
            .map(|(index, image)| (image.module, index))
            .collect();
        if images.is_empty() {
            return;
        }

        let _span = crate::span!("record symbolize");
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
                .and_then(|&image| image_of(&self.images, image).info.as_ref())
            else {
                continue;
            };

            let lines = symbolize(&info.image, ImageAddress::new(address));
            let inputs: Vec<LineInput<'_>> = lines.iter().map(OwnedLine::input).collect();
            self.builder.symbolize(frame, module, &inputs);
        }
    }
}

/// Whether process `pid` began by `CLOCK_MONOTONIC` nanoseconds `time`, as
/// its start in `/proc`, in ticks of the boot clock, shows. A pid taken in
/// the same tick as `time` is not told apart.
fn began_by(pid: u32, time: i64) -> bool {
    use nix::time::{ClockId, clock_gettime};

    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // The 22nd field, the start, is the 20th after the name's last ')'.
    let ticks = stat
        .rfind(')')
        .and_then(|end| stat.get(end + 1..))
        .and_then(|rest| rest.split_whitespace().nth(19))
        .and_then(|ticks| ticks.parse::<i128>().ok());
    let per_second = nix::unistd::sysconf(nix::unistd::SysconfVar::CLK_TCK)
        .ok()
        .flatten()
        .map(i128::from)
        .filter(|ticks| *ticks > 0);
    let nanoseconds = |clock| {
        clock_gettime(clock)
            .ok()
            .map(|time| i128::from(time.tv_sec()) * 1_000_000_000 + i128::from(time.tv_nsec()))
    };
    let (Some(ticks), Some(per_second), Some(boot), Some(monotonic)) = (
        ticks,
        per_second,
        nanoseconds(ClockId::CLOCK_BOOTTIME),
        nanoseconds(ClockId::CLOCK_MONOTONIC),
    ) else {
        return false;
    };

    // The boot clock runs ahead of the monotonic one by the time the
    // machine slept.
    let began = ticks * 1_000_000_000 / per_second - (boot - monotonic);
    began <= i128::from(time)
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
        let symbol = location
            .symbol
            .as_ref()
            .and_then(|symbol| image.symbol(symbol.symbol));
        lines.push(line(
            physical_name(&function, symbol),
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
    } else if let Some(start) = unnamed_function(image, address) {
        // Code no symbol names, as the vDSO's own helpers, is still one
        // function per entry of its call-frame information: its samples
        // are counted together, named for where the function begins.
        let module = image.path().file_name().map_or_else(
            || image.path().to_string_lossy(),
            |name| name.to_string_lossy(),
        );
        let name = Arc::from(format!("<unnamed {module}+{start:#x}>"));
        lines.push(line(name, CodeRole::Ordinary, None, source.as_ref()));
    }

    lines
}

/// Where the function at `address` begins, by the image's call-frame
/// information when any entry describes it, or else where the function
/// whose frames were proved there begins.
fn unnamed_function(image: &ModuleImage, address: ImageAddress) -> Option<u64> {
    let view = crate::image::unwind::UnwindView::new(image.tables());
    [view.eh_frame_index(), view.debug_frame_index()]
        .into_iter()
        .find_map(|index| index.function_start(address.get()))
        .or_else(|| view.proved_function_start(address))
}

/// The name of the function code belongs to. DWARF of line tables alone,
/// as Chromium builds, names a C++ function without its scopes or its
/// linkage name, as `spin`; the symbol holding its code then qualifies
/// it, as `names::Worker::spin`, when it ends in that name.
fn physical_name(function: &crate::FunctionInfo, symbol: Option<crate::Symbol<'_>>) -> Arc<str> {
    if function.linkage_name.is_none()
        && let Some(named) =
            symbol.and_then(|symbol| crate::demangle::qualified_name(symbol.name()))
        && named
            .strip_suffix(&*function.name)
            .is_some_and(|scopes| scopes.ends_with("::"))
    {
        return named.into();
    }

    qualified(function)
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

pub use crate::backend::perf::sys::thread_cpu;

/// In development builds, a directory where a file `NAME.hold` holds the
/// debug-information load of every module whose file is named `NAME` until
/// the file is removed, so that a test can make a load as slow as it needs.
/// microscope writes `NAME.held` there as the load begins to wait.
#[cfg(debug_assertions)]
const HOLD_LOADS: &str = "MICROSCOPE_TEST_HOLD_LOADS";

/// Waits while the test that set [`HOLD_LOADS`] holds `path`'s load, for at
/// most a minute, so that a failed test can't hang the recording.
#[cfg(debug_assertions)]
fn hold_load(path: &std::path::Path) {
    let (Some(directory), Some(name)) = (std::env::var_os(HOLD_LOADS), path.file_name()) else {
        return;
    };

    let directory = std::path::Path::new(&directory);
    let name = name.to_string_lossy();
    let hold = directory.join(format!("{name}.hold"));
    if !hold.exists() {
        return;
    }

    record!("record: the test holds the load of {}", path.display());
    let _ = std::fs::write(directory.join(format!("{name}.held")), b"");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while hold.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_is_read_from_argv_at_a_comm_or_fork_before_any_sample() {
        let pid = Pid::new(std::process::id());
        let expected = std::fs::read(format!("/proc/{pid}/cmdline"))
            .unwrap()
            .split(|byte| *byte == 0)
            .filter(|argument| !argument.is_empty())
            .map(String::from_utf8_lossy)
            .collect::<Vec<_>>()
            .join(" ");
        let time = super::super::session::monotonic();
        let records = [
            decode::Record::Comm {
                pid,
                tid: pid.leader(),
                name: b"test runner",
                exec: true,
                time,
            },
            decode::Record::Fork {
                pid,
                ppid: Pid::new(1),
                tid: pid.leader(),
                ptid: Tid::new(1),
                time,
            },
        ];

        for record in records {
            let mut processor = Processor::new(DebugFileSearch::default(), 64);
            processor.process(record);
            let profile = super::super::Profile {
                capture: processor.finish(),
            };
            assert_eq!(profile.command(0), Some(expected.as_str()));
            assert_eq!(
                profile.command_identity(0),
                Some(super::super::CommandIdentity::Argv(&expected))
            );
        }
    }

    #[test]
    fn command_from_before_exec_is_not_kept_when_the_new_program_is_gone() {
        let mut processor = Processor::new(DebugFileSearch::default(), 64);
        let pid = Pid::new(u32::MAX);
        let process = processor.builder.process(pid, 1_000);
        processor
            .builder
            .setting(&super::super::command_setting(process), "old program --old");
        processor.builder.setting(
            &super::super::arguments_setting(process),
            r#"["old program","--old"]"#,
        );
        processor.process(decode::Record::Comm {
            pid,
            tid: pid.leader(),
            name: b"new program",
            exec: true,
            time: 2_000,
        });
        let profile = super::super::Profile {
            capture: processor.finish(),
        };
        assert_eq!(profile.command(process), None);
        assert_eq!(profile.inspect().processes[process as usize].command, None);
        let read = super::super::Profile::from_bytes(&profile.to_bytes().unwrap())
            .unwrap()
            .profile;
        assert_eq!(
            read.command_identity(process),
            Some(super::super::CommandIdentity::Comm("new program"))
        );
    }

    #[test]
    fn command_identity_uses_only_the_process_or_leader_comm() {
        let mut processor = Processor::new(DebugFileSearch::default(), 64);
        let pid = Pid::new(u32::MAX);
        processor.process(decode::Record::Comm {
            pid,
            tid: Tid::new(123),
            name: b"worker role",
            exec: false,
            time: 1_000,
        });
        let worker_only = super::super::Profile {
            capture: processor.snapshot(),
        };
        assert_eq!(worker_only.command_identity(0), None);

        processor.process(decode::Record::Comm {
            pid,
            tid: pid.leader(),
            name: b"truncated-comm-",
            exec: false,
            time: 2_000,
        });
        let profile = super::super::Profile {
            capture: processor.finish(),
        };
        let read = super::super::Profile::from_bytes(&profile.to_bytes().unwrap())
            .unwrap()
            .profile;
        assert_eq!(read.command(0), None);
        assert_eq!(
            read.command_identity(0),
            Some(super::super::CommandIdentity::Comm("truncated-comm-"))
        );
        assert_eq!(read.command_identity(1), None);
    }

    #[test]
    fn command_identity_of_a_fork_without_exec_uses_the_inherited_leader_comm() {
        let mut processor = Processor::new(DebugFileSearch::default(), 64);
        let parent = Pid::new(u32::MAX - 1);
        let child = Pid::new(u32::MAX);
        processor.process(decode::Record::Comm {
            pid: parent,
            tid: parent.leader(),
            name: b"parent name",
            exec: true,
            time: 1_000,
        });
        processor.process(decode::Record::Fork {
            pid: child,
            ppid: parent,
            tid: child.leader(),
            ptid: parent.leader(),
            time: 2_000,
        });
        let profile = super::super::Profile {
            capture: processor.finish(),
        };
        assert_eq!(profile.command(1), None);
        assert_eq!(
            profile.command_identity(1),
            Some(super::super::CommandIdentity::Comm("parent name"))
        );
    }

    #[test]
    fn command_is_refreshed_at_the_first_sample_only() {
        let mut processor = Processor::new(DebugFileSearch::default(), 64);
        let pid = Pid::new(std::process::id());
        let time = super::super::session::monotonic();
        processor.process(decode::Record::Comm {
            pid,
            tid: pid.leader(),
            name: b"test runner",
            exec: true,
            time,
        });
        processor.builder.setting("command 0", "earlier argv");

        let mut bytes = 9_u32.to_le_bytes().to_vec();
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&72_u16.to_le_bytes());
        for word in [0, u64::from(pid.get()) * 0x1_0000_0001, time, 0, 1, 0, 0, 0] {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        processor.process(decode::next(&bytes).unwrap().unwrap().0);
        let profile = super::super::Profile {
            capture: processor.snapshot(),
        };
        assert!(profile.command(0).is_some());
        assert_ne!(profile.command(0), Some("earlier argv"));

        processor.builder.setting("command 0", "later argv");
        processor.process(decode::next(&bytes).unwrap().unwrap().0);
        let profile = super::super::Profile {
            capture: processor.finish(),
        };
        assert_eq!(profile.command(0), Some("later argv"));
    }

    /// What the loader's workers spend is the recording's cost too, though
    /// the processor only waits for it: loading a JIT compiler's objects
    /// once cost seconds of their CPU time that no report counted.
    #[test]
    fn the_loaders_work_is_counted_as_recording() {
        use rayon::prelude::*;
        let processor = Processor::new(DebugFileSearch::default(), 64);
        let before = processor.cpu();
        let burn = |_| {
            let began = thread_cpu();
            while thread_cpu().saturating_sub(began) < std::time::Duration::from_millis(30) {}
        };
        crate::pool::install(|| (0..4).into_par_iter().for_each(burn)).expect("the pool");
        let spent = processor.cpu().saturating_sub(before);
        assert!(spent >= std::time::Duration::from_millis(110), "{spent:?}");
    }

    /// A mapping of `pages` pages from page `start`, of image `image`
    /// at a bias that tells it from every other.
    fn mapping(step: u32, start: usize, pages: usize, image: u32) -> Mapped {
        Mapped {
            start: start as u64 * 4096,
            end: (start + pages) as u64 * 4096,
            image: image as usize,
            loaded: LoadedModule {
                id: ModuleId::new(step),
                image: ModuleImageId::new(image),
                load_bias: LoadBias::new(u64::from(step) * 0x1000),
                window: None,
            },
        }
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(256))]

        /// After any mappings, each page is in the module of the last
        /// mapping that covered it, in a piece of the same image and bias;
        /// the pieces are apart, none empty; and a mapping keeps the order
        /// of the pieces it leaves, which a copy's place first relies on.
        #[test]
        fn a_mapping_leaves_each_page_to_the_last_that_covered_it(
            steps in proptest::collection::vec((0..48_usize, 1..20_usize, 0..3_u32), 1..16),
        ) {
            use proptest::prelude::*;
            let mut mapped: Vec<Mapped> = Vec::new();
            let mut last: Vec<Option<u32>> = vec![None; 48];

            for (step, &(start, pages, image)) in (0..).zip(&steps) {
                let end = (start + pages).min(48);
                let before: Vec<u32> = mapped.iter().map(|piece| piece.loaded.id.get()).collect();
                uncover(&mut mapped, start as u64 * 4096, end as u64 * 4096);

                // The pieces left, each mapping's together, in their order.
                let mut after: Vec<u32> = mapped.iter().map(|piece| piece.loaded.id.get()).collect();
                after.dedup();
                let mut order = before.iter();
                prop_assert!(after.iter().all(|id| order.any(|earlier| earlier == id)), "{:?} then {:?}", before, after);

                mapped.push(mapping(step, start, end - start, image));
                for page in &mut last[start..end] {
                    *page = Some(step);
                }
            }

            prop_assert!(mapped.iter().all(|piece| piece.start < piece.end));

            for (page, last) in last.iter().enumerate() {
                let address = page as u64 * 4096;
                let holding: Vec<&Mapped> = mapped
                    .iter()
                    .filter(|piece| piece.start <= address && address < piece.end)
                    .collect();
                let expected = last.map(|step| {
                    let piece = mapping(step, 0, 1, steps[step as usize].2);
                    (piece.image, piece.loaded)
                });
                prop_assert!(holding.len() <= 1, "page {} is in {:?}", page, holding);
                prop_assert_eq!(holding.first().map(|piece| (piece.image, piece.loaded)), expected, "page {}", page);
            }
        }
    }

    #[test]
    fn task_snapshot_notes_are_kept_without_counting_as_failures() {
        let mut processor = Processor::new(DebugFileSearch::default(), 64);
        let pid = Pid::new(7);
        processor.task_snapshot(
            pid,
            (1_000, 1_100),
            100,
            &Snapshot {
                notes: vec!["first note".to_owned(), "second note".to_owned()],
                ..Snapshot::default()
            },
        );
        processor.live_task_snapshot(
            pid,
            (2_000, 2_100),
            100,
            &Snapshot {
                notes: vec!["live note".to_owned()],
                problems: vec!["read failed".to_owned()],
                ..Snapshot::default()
            },
        );
        let profile = super::super::Profile {
            capture: processor.finish(),
        };
        let profile = super::super::Profile::from_bytes(&profile.to_bytes().unwrap())
            .unwrap()
            .profile;
        assert_eq!(profile.task_snapshots(), (2, 100, 1));
        assert_eq!(
            profile.task_snapshot_problems(),
            [
                (1_000, "first note".to_owned()),
                (1_000, "second note".to_owned()),
                (2_000, "live note".to_owned()),
                (2_000, "read failed".to_owned()),
            ]
        );
    }

    #[test]
    fn changed_threads_keep_their_time_and_ids_without_counting_as_failures() {
        let mut processor = Processor::new(DebugFileSearch::default(), 64);
        let pid = Pid::new(7);
        processor.live_task_snapshot(
            pid,
            (1_000, 1_100),
            100,
            &Snapshot {
                changed: 3,
                changed_threads: vec![12, 34],
                ..Snapshot::default()
            },
        );
        processor.live_task_snapshot(pid, (2_000, 2_100), 100, &Snapshot::default());
        processor.live_task_snapshot(
            pid,
            (3_000, 3_100),
            100,
            &Snapshot {
                changed: 1,
                changed_threads: vec![56],
                problems: vec!["first problem".to_owned(), "second problem".to_owned()],
                ..Snapshot::default()
            },
        );
        let profile = super::super::Profile {
            capture: processor.finish(),
        };
        let profile = super::super::Profile::from_bytes(&profile.to_bytes().unwrap())
            .unwrap()
            .profile;
        assert_eq!(profile.live_snapshots(), (3, 4, 4));
        assert_eq!(profile.task_snapshots(), (3, 0, 2));
        assert_eq!(
            profile.task_snapshot_problems(),
            [
                (1_000, "threads changed while read: 12, 34".to_owned()),
                (3_000, "threads changed while read: 56".to_owned()),
                (3_000, "first problem, and 1 more".to_owned()),
            ]
        );
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
                ptid: Tid::new(1),
                time: 1_000,
            },
            decode::Record::Fork {
                pid: exited,
                ppid: exited,
                tid: napper,
                ptid: exited.leader(),
                time: 2_000,
            },
            decode::Record::Fork {
                pid: living,
                ppid: Pid::new(1),
                tid: living.leader(),
                ptid: Tid::new(1),
                time: 2_000,
            },
            decode::Record::Fork {
                pid: living,
                ppid: living,
                tid: sleeper,
                ptid: living.leader(),
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
    /// still name its code. It changes the process's modules, so what they
    /// say of its interpreter is read again; anonymous code a JIT compiler
    /// maps, and maps again over the objects it registered, changes none.
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
        let modules = processor.processes[&pid].mappings;

        let jit = |time| decode::Mmap {
            pid,
            start: 0x7e00_0000_0000,
            length: 0x1000,
            file_offset: 0,
            inode: None,
            executable: true,
            path: b"//anon",
            time,
        };
        processor.process(decode::Record::Mmap(jit(1_100)));
        let object = crate::debug_info::gdb_jit::test_image(0x7e00_0000_0000, 64);
        processor.jit_object_module(
            pid,
            1_200,
            &JitCode {
                start: 0x7e00_0000_0000,
                end: 0x7e00_0000_0040,
                path: "[jit]".to_owned(),
                bytes: object.into(),
            },
        );
        processor.process(decode::Record::Mmap(jit(1_300)));
        assert_eq!(processor.processes[&pid].mappings, modules);

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
        assert_ne!(processor.processes[&pid].mappings, modules);
        let pieces = processor.processes[&pid]
            .mapped
            .iter()
            .filter(|mapped| mapped.start >= 0x7f00_0000_0000)
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
    /// A thread blocked under V8's code is never sampled there, so where a
    /// ring lost the records that mapped that code, only a live read of
    /// the thread finds it: the code that holds a frame the read found in
    /// no mapping is mapped then, as a sample's would be.
    #[test]
    fn code_a_live_read_found_in_no_mapping_is_mapped() {
        use std::io::BufRead as _;

        struct Killed(std::process::Child);
        impl Drop for Killed {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        let node = std::path::Path::new(
            &std::env::var_os("MICROSCOPE_NODE").expect("MICROSCOPE_NODE; run in the dev shell"),
        )
        .join("26/node");
        let mut child = Killed(
            std::process::Command::new(node)
                .args([
                    "-e",
                    "process.stdout.write('ready\\n'); \
                     Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 60000)",
                ])
                .stdout(std::process::Stdio::piped())
                .spawn()
                .expect("run node"),
        );
        let stdout = child.0.stdout.take().expect("its output");
        let (sent, ready) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let line = std::io::BufReader::new(stdout).lines().next();
            let _ = sent.send(line);
        });
        let line = ready
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("node says it is ready");
        assert_eq!(line.map(Result::ok), Some(Some("ready".to_owned())));
        let pid = Pid::new(child.0.id());

        // Every mapping's record arrives but those of anonymous code, which
        // the ring lost.
        let mut processor = Processor::new(DebugFileSearch::default(), 64);
        let mappings = perf::mappings(pid.get()).expect("node's mappings");
        let mut anonymous = Vec::new();
        for mapping in &mappings {
            let mmap = decode::Mmap {
                pid,
                start: mapping.start,
                length: mapping.end - mapping.start,
                file_offset: mapping.file_offset,
                inode: Some(mapping.inode),
                executable: mapping.executable,
                path: &mapping.path,
                time: 1_000,
            };
            if perf::jit::is_anonymous_code(&mmap) {
                anonymous.push(mapping.start..mapping.end);
            } else {
                processor.mmap(&mmap);
            }
        }
        let code = anonymous
            .iter()
            .max_by_key(|range| range.end - range.start)
            .expect("V8 maps anonymous code")
            .start
            + 0x10;
        let covers = |processor: &Processor| {
            processor
                .mapped_code(pid)
                .iter()
                .any(|piece| piece.start <= code && code < piece.end)
        };
        assert!(!covers(&processor), "mapped before any read found it");

        // Records stamped before the read may not be processed yet, so
        // the code is mapped once they are, as a lost mapping is.
        processor.map_live_code(pid, 2_000, &[code]);
        processor.ingest(&[], Some(1_999));
        assert!(
            !covers(&processor),
            "mapped before every record before the read was processed"
        );
        processor.ingest(&[], Some(2_000));
        assert!(
            covers(&processor),
            "a live read's frame at {code:#x} is still in no mapping"
        );
    }

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
        let early = processor.found_probes(pid).found.len();

        processor.ingest(&[], None);
        let found = processor.found_probes(pid).found;
        let mapped = processor.processes[&pid].mapped.len();

        processor.process(decode::Record::Lost {
            count: 1,
            time: super::super::session::monotonic(),
        });
        processor.ingest(&[], None);
        let again = (
            processor.found_probes(pid).found.len(),
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

    /// A module made from a perf map names only what the map named then.
    /// A sample in code the program made since, in the same anonymous
    /// range, reads the map again for its name, as for V8's code.
    #[test]
    fn a_perf_maps_module_reads_the_map_again_for_code_it_does_not_name() {
        #[repr(align(64))]
        struct Code([u8; 64]);
        static CODE: Code = Code([0xcc; 64]);
        struct Removed(String);
        impl Drop for Removed {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }

        let own = std::process::id();
        let (pid, tid) = (Pid::new(own), Tid::new(own));
        let start = CODE.0.as_ptr() as u64;
        let path = format!("/tmp/perf-{own}.map");
        std::fs::write(&path, format!("{start:x} 10 a\n")).expect("write the map");
        let _removed = Removed(path.clone());

        // A sample whose kernel chain holds `address` alone.
        let sample = |address: u64, time: u64| {
            let mut body = Vec::new();
            for word in [
                1,
                u64::from(own) | (u64::from(own) << 32),
                time,
                3,
                1_000,
                1,
            ] {
                body.extend_from_slice(&word.to_le_bytes());
            }
            for word in [address, 0, 0] {
                body.extend_from_slice(&word.to_le_bytes());
            }
            let mut record = 9_u32.to_le_bytes().to_vec();
            record.extend_from_slice(&0_u16.to_le_bytes());
            record.extend_from_slice(&u16::try_from(8 + body.len()).expect("small").to_le_bytes());
            record.extend_from_slice(&body);
            record
        };

        let mut processor = Processor::new(DebugFileSearch::default(), 64);
        let now = super::super::session::monotonic();
        processor.process(decode::Record::Fork {
            pid,
            ppid: Pid::new(1),
            tid,
            ptid: Tid::new(1),
            time: now,
        });
        let anonymous = perf::Mapped {
            start,
            end: start + 64,
            file_offset: 0,
            inode: 0,
            executable: true,
            writable: false,
            path: b"//anon".to_vec(),
        };
        let mut records = mmap_record(own, &anonymous, now + 1);
        records.extend(sample(start + 8, now + 2));
        processor.ingest(&records, None);

        // The program names b after the map was read, and runs it later.
        let mut map = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open the map");
        std::io::Write::write_all(&mut map, format!("{:x} 10 b\n", start + 0x20).as_bytes())
            .expect("append to the map");
        let later = super::super::session::monotonic() + 1_000_000_000;
        processor.ingest(&sample(start + 0x28, later), None);

        let profile = super::super::Profile {
            capture: processor.finish(),
        };
        let leaves: Vec<Option<String>> = profile
            .stacks()
            .into_iter()
            .map(|stack| stack.functions.first().cloned())
            .collect();
        assert_eq!(leaves, [Some("a".to_owned()), Some("b".to_owned())]);
    }

    /// The bytes of a perf record of process `pid` mapping `mapped`, as
    /// the kernel writes it, stamped `time`.
    fn mmap_record(pid: u32, mapped: &perf::Mapped, time: u64) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&pid.to_le_bytes());
        body.extend_from_slice(&pid.to_le_bytes());
        for word in [mapped.start, mapped.end - mapped.start, mapped.file_offset] {
            body.extend_from_slice(&word.to_le_bytes());
        }
        body.extend_from_slice(&[0; 8]);
        body.extend_from_slice(&mapped.inode.to_le_bytes());
        body.extend_from_slice(&[0; 8]);
        body.extend_from_slice(&5_u32.to_le_bytes());
        body.extend_from_slice(&2_u32.to_le_bytes());
        body.extend_from_slice(&mapped.path);
        body.resize(body.len().next_multiple_of(8) + 8, 0);
        for word in [u64::from(pid) | (u64::from(pid) << 32), time, 3, 1] {
            body.extend_from_slice(&word.to_le_bytes());
        }
        let mut record = 10_u32.to_le_bytes().to_vec();
        record.extend_from_slice(&0_u16.to_le_bytes());
        record.extend_from_slice(&u16::try_from(8 + body.len()).expect("small").to_le_bytes());
        record.extend_from_slice(&body);
        record
    }

    /// A live read finds code by every mapping recorded before it: one the
    /// last drain held back, past a watermark a slow drain left behind, as
    /// V8's first code is while the drain loads Node's debug information,
    /// and one still in the rings. It drains only while what was processed
    /// lags, and no more than so many times.
    #[test]
    fn a_live_read_finds_code_mapped_before_it_whatever_the_drain_held_back() {
        const MS: u64 = 1_000_000;
        let own = std::process::id();
        let pid = Pid::new(own);
        let files: Vec<perf::Mapped> = perf::mappings(own)
            .expect("this process's mappings")
            .into_iter()
            // Libraries, whose debug information reads at once.
            .filter(|mapped| {
                mapped.executable
                    && mapped.path.starts_with(b"/")
                    && mapped.path.windows(3).any(|part| part == b".so")
            })
            .take(2)
            .collect();
        let [first, second] = files.as_slice() else {
            panic!("two executable file mappings: {files:?}");
        };
        let covers = |code: &[super::super::tasks::MappedCode], mapped: &perf::Mapped| {
            code.iter()
                .any(|piece| (piece.start..piece.end).contains(&mapped.start))
        };

        let mut processor = Processor::new(DebugFileSearch::default(), 64);
        processor.ingest(&mmap_record(own, first, 1_010 * MS), Some(1_000 * MS));
        assert!(
            !covers(&processor.mapped_code(pid), first),
            "the drain processed a record past its watermark"
        );

        // What was processed lags by less than the slack: no drain.
        let mut drained = 0;
        let code = processor.code_for_read(pid, || 1_001 * MS, |_| drained += 1);
        assert_eq!(drained, 0, "drained though nothing lagged");
        assert!(!covers(&code, first), "{code:?}");

        // A slow drain left the record held back.
        let code = processor.code_for_read(pid, || 1_050 * MS, |_| drained += 1);
        assert_eq!(drained, 1);
        assert!(covers(&code, first), "held back: {code:?}");

        // A record still in the rings.
        let mut rings = mmap_record(own, second, 1_060 * MS);
        let code = processor.code_for_read(
            pid,
            || 1_100 * MS,
            |blocks| {
                blocks.filled.push(std::mem::take(&mut rings));
            },
        );
        assert!(covers(&code, second), "drained: {code:?}");

        // Each drain ran long, but the read waits for no more than so many.
        let now = std::cell::Cell::new(2_000 * MS);
        let mut drained = 0;
        processor.code_for_read(
            pid,
            || {
                now.set(now.get() + 100 * MS);
                now.get()
            },
            |_| drained += 1,
        );
        assert_eq!(drained, READ_DRAINS);
    }

    impl Processor {
        /// [`Processor::ingest_drained_after_probes`] of `batch`.
        fn ingest_after_probes(&mut self, batch: &[u8], watermark: u64) {
            let mut batch = batch.to_vec();
            self.ingest_drained_after_probes(
                |blocks| blocks.filled.push(std::mem::take(&mut batch)),
                watermark,
            );
        }
    }

    /// A drain that loads after the probes finds the instrumentation of
    /// every module mapped up to its watermark before any loads, then stops
    /// at the first mapping of a module not yet loaded, holding it and
    /// every record after it, and says that what it processed ends before
    /// it. Once the modules load, the next drain goes on from that mapping.
    /// A drain to the end, as a recording's last is, loads as it goes.
    #[test]
    fn a_drain_finds_probes_then_stops_at_a_module_to_load() {
        use std::io::{BufRead as _, Write as _};
        const MS: u64 = 1_000_000;

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
        let mut names = child.stdin.take().expect("its input");
        let mut lines = std::io::BufReader::new(child.stdout.take().expect("its output")).lines();
        writeln!(names, "ready").expect("name a site");
        lines.next().expect("a site").expect("its line");

        let code: Vec<perf::Mapped> = perf::mappings(pid.get())
            .expect("its mappings")
            .into_iter()
            .filter(|mapped| mapped.executable && mapped.path.starts_with(b"/"))
            .collect();
        let library = code
            .iter()
            .find(|mapped| mapped.path.windows(3).any(|part| part == b".so"))
            .expect("a library");
        let own = code
            .iter()
            .find(|mapped| mapped.path == program.as_os_str().as_encoded_bytes())
            .expect("the program's code");

        // The library is mapped first, as the loader's are before a
        // library's that the program opens holds its instrumentation.
        let mut batch = mmap_record(pid.get(), library, 1_000 * MS);
        batch.extend(mmap_record(pid.get(), own, 1_001 * MS));
        batch.extend(bare_sample(1_002 * MS));
        let mapped = |processor: &Processor| {
            let code = processor.mapped_code(pid);
            [library, own].map(|mapped| {
                code.iter()
                    .any(|piece| (piece.start..piece.end).contains(&mapped.start))
            })
        };

        let mut processor = Processor::new(DebugFileSearch::default(), 64);
        processor.ingest_after_probes(&batch, 2_000 * MS);
        let found = processor.found_probes(pid).found;
        assert_eq!(
            found
                .iter()
                .map(|found| std::path::Path::new(&*found.path))
                .collect::<Vec<_>>(),
            [program.as_path()]
        );
        assert_eq!(mapped(&processor), [false, false]);
        assert!(processor.images_pending() && processor.stopped_at_load());
        assert_eq!(processor.processed, 1_000 * MS - 1);
        assert_eq!(processor.processed_to, 1_000 * MS - 1);
        assert!(processor.snapshot().samples.is_empty());

        // The library loads alone, and the drain stops at the program's
        // code, which the drain before found but did not load.
        processor.load_stopped();
        assert!(processor.images_pending());
        processor.ingest_after_probes(&[], 2_000 * MS);
        assert_eq!(mapped(&processor), [true, false]);
        assert!(processor.stopped_at_load());
        assert_eq!(processor.processed, 1_001 * MS - 1);

        processor.load_stopped();
        assert!(!processor.images_pending());
        processor.ingest_after_probes(&[], 2_000 * MS);
        assert_eq!(mapped(&processor), [true, true]);
        assert!(!processor.stopped_at_load());
        assert_eq!(processor.processed, 2_000 * MS);
        assert_eq!(processor.snapshot().samples.len(), 1);
        assert!(processor.found_probes(pid).found.is_empty(), "found twice");

        let mut at_once = Processor::new(DebugFileSearch::default(), 64);
        at_once.ingest(&batch, Some(2_000 * MS));
        assert_eq!(mapped(&at_once), [true, true]);
        assert!(!at_once.images_pending() && !at_once.stopped_at_load());
        assert_eq!(at_once.found_probes(pid).found.len(), 1);
        assert_eq!(at_once.snapshot().samples.len(), 1);

        drop(names);
        child.wait().expect("its exit");
    }

    /// What a process maps after it executes a program is the new
    /// program's: a drain finds no instrumentation of it ahead of the exec,
    /// which ends what was found before.
    #[test]
    fn a_drain_finds_no_probes_ahead_of_an_exec() {
        const MS: u64 = 1_000_000;
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
        let mut names = child.stdin.take().expect("its input");
        let mut lines = std::io::BufRead::lines(std::io::BufReader::new(
            child.stdout.take().expect("its output"),
        ));
        std::io::Write::write_all(&mut names, b"ready\n").expect("name a site");
        lines.next().expect("a site").expect("its line");
        let code: Vec<perf::Mapped> = perf::mappings(pid.get())
            .expect("its mappings")
            .into_iter()
            .filter(|mapped| mapped.executable && mapped.path.starts_with(b"/"))
            .collect();
        let library = code
            .iter()
            .find(|mapped| mapped.path.windows(3).any(|part| part == b".so"))
            .expect("a library");
        let own = code
            .iter()
            .find(|mapped| mapped.path == program.as_os_str().as_encoded_bytes())
            .expect("the program's code");

        // PERF_RECORD_COMM with the exec bit, and its sample_id trailer.
        let mut exec = vec![0; 64];
        exec[..4].copy_from_slice(&3_u32.to_le_bytes());
        exec[4..6].copy_from_slice(&0x2000_u16.to_le_bytes());
        exec[6..8].copy_from_slice(&64_u16.to_le_bytes());
        exec[8..12].copy_from_slice(&pid.get().to_le_bytes());
        exec[12..16].copy_from_slice(&pid.get().to_le_bytes());
        exec[16..23].copy_from_slice(b"probes\0");
        exec[40..48].copy_from_slice(&(1_001 * MS).to_le_bytes());

        let mut batch = mmap_record(pid.get(), library, 1_000 * MS);
        batch.extend(exec);
        batch.extend(mmap_record(pid.get(), own, 1_002 * MS));
        let mut processor = Processor::new(DebugFileSearch::default(), 64);
        processor.ingest_after_probes(&batch, 2_000 * MS);
        let ahead = processor.found_probes(pid);

        while processor.stopped_at_load() {
            processor.load_stopped();
            processor.ingest_after_probes(&[], 2_000 * MS);
        }
        let after = processor.found_probes(pid);

        drop(names);
        child.wait().expect("its exit");
        assert!(ahead.found.is_empty() && !ahead.executed, "{ahead:?}");
        assert!(after.executed && after.skipped.is_empty(), "{after:?}");
        assert_eq!(after.found.len(), 1, "{after:?}");
    }

    /// Once a recording has loaded a program's modules and seen its stacks,
    /// processing a sample allocates nothing: not when it is held back past
    /// a watermark, nor when it is unwound and added to the capture, whose
    /// list of samples alone grows. Samples of a program whose main thread
    /// spins below frames no frame pointer links, copying 16 KiB of stack,
    /// are processed again and again.
    #[test]
    fn a_sample_allocates_nothing_once_its_stack_was_seen() {
        const ROUNDS: usize = 4;
        let options = perf::EventOptions {
            source: perf::Source::Clock {
                period: std::time::Duration::from_millis(1),
            },
            stack_bytes: 16 * 1024,
            kernel: false,
            ring_pages: 256,
            switches: false,
        };
        let program = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("build/test-programs/profile-stacks-gcc-o2-nofp");
        let mut command = std::process::Command::new(program);
        command.arg("30000000");
        let perf::Launched { events, exit, .. } =
            perf::launch(command, &options, perf::LaunchOptions::default(), |_, _| {
                Ok(())
            })
            .expect("launch the program");

        // Read as the program runs, as recording does: the rings may be far
        // smaller than asked when other recordings hold the user's share of
        // locked memory, and would drop most samples by the program's end.
        let status = events
            .read_while(|| exit.join())
            .expect("the launch thread ends")
            .expect("the program exits");
        assert!(status.success(), "{status}");

        let mut records = Vec::new();
        events.drain(&mut records);
        let (mut samples, mut times) = (Vec::new(), Vec::new());
        let mut rest = records.as_slice();
        while let Some(Ok((record, length))) = decode::next(rest) {
            if let decode::Record::Sample(sample) = record {
                samples.extend_from_slice(&rest[..length]);
                times.push(sample.time);
            }
            rest = &rest[length..];
        }
        assert!(times.len() > 50, "{} samples", times.len());

        let mut processor = Processor::new(DebugFileSearch::default(), 64);
        processor.ingest(&records, None);
        let middle = times[times.len() / 2];
        let replay = |processor: &mut Processor| {
            processor.ingest(&samples, Some(middle));
            processor.ingest(&[], None);
        };
        replay(&mut processor);

        let before = crate::timings::alloc::thread_allocated();
        for _ in 0..ROUNDS {
            replay(&mut processor);
        }
        let allocated = crate::timings::alloc::thread_allocated().since(&before);
        let capture = processor.finish();
        assert_eq!(capture.samples.len(), (ROUNDS + 2) * times.len());
        // The capture's sample list doubles at most this often.
        assert!(
            allocated.blocks <= 4,
            "{allocated:?} over {} samples",
            ROUNDS * times.len()
        );
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

    #[test]
    fn kernel_samples_keep_the_user_stack_and_a_sample_time_syscall_leaf() {
        for (kernel, metadata, expected) in [
            (true, Some((0, 0)), Some("[kernel: read]")),
            (true, Some((1, 0)), Some("[kernel: write]")),
            (true, Some((u64::MAX, 0)), Some("[kernel: unknown]")),
            (true, Some((0, 3)), Some("[kernel: unknown]")),
            (true, None, Some("[kernel: unknown]")),
            (false, Some((0, 0)), None),
        ] {
            check_kernel_sample(kernel, metadata, expected);
        }
    }

    fn check_kernel_sample(kernel: bool, metadata: Option<(u64, u32)>, expected: Option<&str>) {
        let mut processor = Processor::new(DebugFileSearch::default(), 64);
        processor.interpret(vec![42]);
        let mut output = frames_record(10_500, false, 0);
        if let Some((syscall, abi)) = metadata {
            // Insert the sampler's kernel trailer before perf's raw padding.
            let at = output.len() - 4;
            let mut trailer = syscall.to_le_bytes().to_vec();
            trailer.extend_from_slice(&abi.to_le_bytes());
            trailer.extend_from_slice(&0_u32.to_le_bytes());
            output.splice(at..at, trailer);
            let size = u16::try_from(output.len()).unwrap();
            output[6..8].copy_from_slice(&size.to_le_bytes());
            output[40..44].copy_from_slice(&60_u32.to_le_bytes());
        }

        let mut rest = Vec::new();
        for word in [
            1000_u64,
            2,
            (-128_i64).cast_unsigned(),
            0xffff_ffff_8100_0000,
            2,
        ] {
            rest.extend_from_slice(&word.to_le_bytes());
        }
        let mut registers = [0_u64; 18];
        registers[7] = 0x7000_0000;
        registers[8] = 0x0040_1000;
        for word in registers.into_iter().chain([0]) {
            rest.extend_from_slice(&word.to_le_bytes());
        }
        let mut sample = perf_record(1, 10_000, &rest);
        sample[4..6].copy_from_slice(&(if kernel { 1_u16 } else { 2 }).to_le_bytes());
        output.extend(sample);
        processor.ingest(&output, Some(9_000));
        processor.ingest(&[], None);
        let capture = processor.finish();
        let frames: Vec<_> = capture
            .stack_frames(capture.samples[0].stack.get())
            .collect();
        if let Some(expected) = expected {
            let leaf = capture.frame_lines(frames[0]);
            assert_eq!(
                capture.frames[frames[0] as usize].kind,
                FrameKind::Kernel as u8
            );
            assert_eq!(
                capture.string(
                    capture.functions[leaf[0].function.get() as usize]
                        .name
                        .get()
                ),
                expected
            );
        } else {
            assert!(
                frames
                    .iter()
                    .all(|frame| capture.frames[*frame as usize].kind != FrameKind::Kernel as u8)
            );
        }
        assert!(frames.iter().any(|frame| {
            let frame = &capture.frames[*frame as usize];
            frame.kind == FrameKind::Native as u8 && frame.address.get() == 0x0040_1000
        }));
        assert!(!frames.iter().any(|frame| capture.frames[*frame as usize].address.get() == 0xffff_ffff_8100_0000));
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
