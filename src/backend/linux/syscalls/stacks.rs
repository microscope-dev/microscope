//! The stacks of system calls (`-k`).
//!
//! Each traced process's modules come from the records of its mappings or
//! from `/proc`. A stack is unwound through them by call-frame information,
//! and each of its addresses is named by module, symbol, and source line.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use foldhash::HashMap;

use super::super::perf::{self, decode};
use crate::backend::bpf::sampler::{self, RawPyLayout};
use crate::capture::builder::{CaptureBuilder, InterpretedPlace, LineInput};
use crate::capture::records::{FrameKind, NONE, Truncation};
use crate::debug_info::{DebugFileSearch, DebugInfo};
use crate::model::Pid;
use crate::profiler::{
    InterpretedWait, LiveStop, OwnedLine, Sampled, hex, read_record, symbolize_lines,
};
use crate::runtime_model::{
    self, InterpretedStacks, InterpretedThread, RuntimeImage, RuntimeModel,
};
use crate::syscalls::{CallStack, InterpretedCall};
use crate::unwind::sample::{
    FrameRules, SampledFrame, SampledState, StackEnd, unwind_memory, unwind_sample,
};

use crate::unwind::{MemoryReader, UnwindModule};
use crate::{
    CodeRole, DebugFileOptions, ImageAddress, LoadBias, LoadedModule, ModuleId, ModuleImageId,
    VirtualAddress,
};

/// The most frames a stack keeps, as strace's default.
pub const MOST_FRAMES: usize = 256;

/// One module mapped into a process.
#[derive(Clone)]
struct Mapped {
    start: u64,
    end: u64,
    path: Arc<str>,
    loaded: LoadedModule,
    image: Image,

    /// Whether it holds code a runtime generated, which no image holds.
    generated: bool,
}

/// A file's image, if it could be read, and what names the file in a
/// capture.
#[derive(Clone, Default)]
struct Image {
    info: Option<Arc<DebugInfo>>,
    identity: Option<Arc<Identity>>,
}

/// What names a module's file in a capture, as the profiler names it.
#[derive(Debug)]
struct Identity {
    build_id: Option<String>,
    digest: [u8; 16],
    size: u64,
}

impl Identity {
    fn of(data: &[u8]) -> Self {
        let build_id = object::File::parse(data)
            .ok()
            .and_then(|file| object::Object::build_id(&file).ok().flatten().map(hex));
        Self {
            build_id,
            digest: twox_hash::XxHash3_128::oneshot(data).to_le_bytes(),
            size: data.len() as u64,
        }
    }
}

/// One address of a stack, named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// The code the frame runs: the innermost frame's instruction, and in
    /// each frame after, the byte before its return address, inside the
    /// call, as strace names it.
    pub address: VirtualAddress,

    /// The mapped file, as `/proc/PID/maps` names it, and the address
    /// within its image.
    pub module: Option<(Arc<str>, ImageAddress)>,

    /// The function's symbol and the address's offset from it.
    pub symbol: Option<(Arc<str>, u64)>,

    /// The source file and line of the code the frame runs.
    pub source: Option<(String, u64)>,
}

/// Each traced process's modules, and the images they map, loaded once.
///
/// A process's modules come from the records of its mappings, as they
/// happen, when an engine opens them; otherwise from `/proc`, which a
/// process that has ended no longer has.
pub struct Stacks {
    search: DebugFileSearch,
    images: HashMap<(PathBuf, u64), Image>,
    processes: BTreeMap<Pid, Vec<Mapped>>,
    rules: FrameRules,
    next_module: u32,

    /// The identifier the next image read gets, the vDSO's among them.
    next_image: u32,

    /// The vDSO's image, read from microscope's own, which the kernel maps
    /// alike into every process, and the address it links at.
    vdso: Vdso,

    /// The interpreter each module of a process runs, if any, found as a
    /// stack first needs it. Each process has its own, as a model learns
    /// the code objects of the process it reads.
    runtimes: HashMap<(Pid, ModuleId), Option<Arc<dyn RuntimeModel>>>,

    /// The addresses of each process a stack ended at in no module even
    /// after its mappings were read again, which are not read again for
    /// them until the process maps something.
    uncovered: HashMap<Pid, std::collections::BTreeSet<u64>>,
}

impl std::fmt::Debug for Stacks {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Stacks")
            .field("processes", &self.processes.len())
            .field("images", &self.images.len())
            .finish_non_exhaustive()
    }
}

/// Whether the vDSO's image has been read.
#[derive(Clone)]
enum Vdso {
    Unread,
    Read((Image, u64)),
    Unreadable,
}

/// Stacks shared by an engine, which unwinds them, and whatever prints
/// them, so that each image loads once.
pub type SharedStacks = Arc<std::sync::Mutex<Stacks>>;

impl Default for Stacks {
    fn default() -> Self {
        Self::new(&DebugFileOptions::default())
    }
}

/// A live thread's memory, read from outside it.
struct Live(u32);

impl MemoryReader for Live {
    fn read_u64(&mut self, address: VirtualAddress) -> Option<u64> {
        let mut word = [0; 8];
        (perf::read_memory(self.0, address.get(), &mut word).ok()? == 8)
            .then(|| u64::from_le_bytes(word))
    }
}

impl Stacks {
    #[must_use]
    pub fn new(options: &DebugFileOptions) -> Self {
        Self {
            search: DebugFileSearch::new(options),
            images: HashMap::default(),
            processes: BTreeMap::new(),
            rules: FrameRules::default(),
            next_module: 0,
            next_image: 0,
            vdso: Vdso::Unread,
            runtimes: HashMap::default(),
            uncovered: HashMap::default(),
        }
    }

    /// A mapping the process made, from its record.
    pub fn mapped(&mut self, mmap: &decode::Mmap<'_>) {
        let end = mmap.start.saturating_add(mmap.length);
        self.uncovered.remove(&mmap.pid);
        let process = self.processes.entry(mmap.pid).or_default();
        process.retain(|mapped| mapped.end <= mmap.start || mapped.start >= end);
        if let Some(mapped) = self.module(mmap) {
            self.processes.entry(mmap.pid).or_default().push(mapped);
        }
    }

    /// The module a mapping maps, if it is a file's code or the vDSO.
    fn module(&mut self, mmap: &decode::Mmap<'_>) -> Option<Mapped> {
        let (image, bias) = if perf::is_vdso(mmap) {
            let (image, link) = self.vdso()?;
            let start = VirtualAddress::new(mmap.start);
            (image, LoadBias::between(ImageAddress::new(link), start))
        } else {
            let (file, bias) = perf::identify(mmap)?;
            (self.image(&file, mmap.inode.unwrap_or(0)), bias)
        };

        let id = image
            .info
            .as_ref()
            .map_or(ModuleImageId::new(u32::MAX), |info| info.image.id());
        let loaded = LoadedModule {
            id: ModuleId::new(self.next_module),
            image: id,
            load_bias: bias,
            window: None,
        };

        self.next_module += 1;
        Some(Mapped {
            start: mmap.start,
            end: mmap.start.saturating_add(mmap.length),
            path: String::from_utf8_lossy(mmap.path).into(),
            loaded,
            image,
            generated: false,
        })
    }

    fn vdso(&mut self) -> Option<(Image, u64)> {
        match &self.vdso {
            Vdso::Read(vdso) => return Some(vdso.clone()),
            Vdso::Unreadable => return None,
            Vdso::Unread => {}
        }

        let id = self.next_image_id();
        let vdso = perf::own_vdso().and_then(|(data, link)| {
            let path = std::path::Path::new(super::super::vdso::VDSO_NAME);
            let info = crate::debug_info::load_module_bytes(path, &data, id, &self.search).ok()?;
            let image = Image {
                info: Some(Arc::new(info)),
                identity: Some(Arc::new(Identity::of(&data))),
            };

            Some((image, link))
        });
        self.vdso = vdso.clone().map_or(Vdso::Unreadable, Vdso::Read);

        vdso
    }

    /// The processes whose modules are known.
    #[must_use]
    pub fn processes(&self) -> Vec<Pid> {
        self.processes.keys().copied().collect()
    }

    /// Forgets a process's modules, as after it replaced its image: they
    /// are read again when a stack next needs them.
    pub fn forget(&mut self, process: Pid) {
        self.processes.remove(&process);
        self.uncovered.remove(&process);
        self.runtimes.retain(|&(of, _), _| of != process);
    }

    /// Unwinds a stopped thread's stack from its registers by DWARF
    /// number, reading its memory: its instruction, then each return
    /// address, innermost first, and the frames an interpreter runs on it,
    /// which stay as they are while it is stopped.
    /// `thread_pointer` is the thread's, which reading V8's compiled
    /// frames needs.
    pub fn unwind_live(
        &mut self,
        process: Pid,
        thread: u32,
        registers: &[(crate::unwind::DwarfRegister, u64)],
        thread_pointer: Option<u64>,
    ) -> CallStack {
        let instruction = registers.iter().find_map(|&(register, value)| {
            (register == crate::unwind::RETURN_ADDRESS).then_some(value)
        });
        self.cover(process, instruction);
        let (mut frames, mut end) = unwind_memory(
            &self.modules(process),
            registers,
            &mut Live(thread),
            MOST_FRAMES,
        );
        if self.uncover(process, &frames) {
            (frames, end) = unwind_memory(
                &self.modules(process),
                registers,
                &mut Live(thread),
                MOST_FRAMES,
            );
        }
        let runtime = self.interpreter(process);
        let mut stack = stack(&frames, end);
        if let Some((model, loaded)) = &runtime
            && model.owns_generated_code()
        {
            let pointers: BTreeMap<u32, u64> = thread_pointer
                .map(|pointer| (thread, pointer))
                .into_iter()
                .collect();
            let stop =
                LiveStop::for_stopped_thread(process.get(), thread, &pointers, loaded.load_bias);
            stack.interpreted = self
                .code_frames(process, model.as_ref(), loaded, &stop, thread, &frames)
                .into();
            return stack;
        }

        let modules = self.modules(process);
        if let Some((model, loaded)) = runtime
            && let Some(read) = model.interpreted(&LiveStop::for_thread(
                process.get(),
                thread,
                loaded.load_bias,
            ))
        {
            let activations = activations(model.as_ref(), &modules, &frames);
            let thread = crate::ThreadId::new(u64::from(thread));
            let (interpreted, truncation) =
                place(&read.value, thread, &activations, stack.addresses.len());
            stack.interpreted = interpreted.into();
            stack.truncated = stack
                .truncated
                .or_else(|| truncation.map(|reason| reason as u8));
        }

        stack
    }

    /// The interpreter `process` runs, and the load bias of the module
    /// that holds it, if it runs one microscope reads.
    fn interpreter(&mut self, process: Pid) -> Option<(Arc<dyn RuntimeModel>, LoadedModule)> {
        for mapped in self.processes.get(&process)? {
            // A copy of a module's code binds the runtime of the module.
            if mapped.loaded.window.is_some() {
                continue;
            }
            let model = self
                .runtimes
                .entry((process, mapped.loaded.id))
                .or_insert_with(|| {
                    let info = mapped.image.info.as_ref()?;
                    let image: Arc<dyn RuntimeImage + Send + Sync> = Arc::clone(&info.image) as _;
                    if image.python().is_none() && image.v8().is_none() {
                        return None;
                    }
                    let stop = LiveStop::memory(process.get(), mapped.loaded.load_bias);
                    runtime_model::detect(&image)
                        .into_iter()
                        .flatten()
                        .find(|model| {
                            model.owns_generated_code() || model.walked_threads(&stop).is_some()
                        })
                });
            if let Some(model) = model {
                return Some((Arc::clone(model), mapped.loaded));
            }
        }

        None
    }

    /// The functions each frame of `frames`, a stopped thread's, runs where
    /// its code is `model`'s own, as the runtime recorded them, each just
    /// before its frame: V8's builtins in the module `runtime` loaded, and
    /// in copies of it, and the code V8 generated.
    fn code_frames(
        &self,
        process: Pid,
        model: &dyn RuntimeModel,
        runtime: &LoadedModule,
        stop: &LiveStop<'_>,
        thread: u32,
        frames: &[SampledFrame],
    ) -> Vec<InterpretedCall> {
        let stored = model.process_code(stop);
        let mut calls = Vec::new();
        for (index, frame) in frames.iter().enumerate() {
            let Some((generated, registers)) = self.owned(process, model, runtime, &stored, frame)
            else {
                continue;
            };
            // The innermost frame, and one a signal interrupted, are at
            // their instruction, not after a call.
            let after_call = index != 0 && frames[index - 1].role != CodeRole::SignalTrampoline;
            let code = runtime_model::CodeFrame {
                thread: crate::ThreadId::new(u64::from(thread)),
                instruction: if after_call {
                    VirtualAddress::new(frame.address.get().wrapping_add(1))
                } else {
                    frame.address
                },
                after_call,
                generated,
                registers: &registers,
            };
            let Some(described) = model.code_frame(stop, &code) else {
                continue;
            };
            let before = u32::try_from(index).unwrap_or(u32::MAX);
            calls.extend(described.functions.iter().map(|function| InterpretedCall {
                before,
                function: Arc::clone(&function.function),
                file: function.place.as_ref().map(|place| Arc::clone(&place.path)),
                line: function.place.as_ref().map_or(0, |place| place.line),
                first_line: function.first_line,
            }));
        }
        calls
    }

    /// Whether `frame`'s code is `model`'s own, and if so whether the
    /// runtime generated it, with the registers the runtime reads it by:
    /// its frame pointer.
    fn owned(
        &self,
        process: Pid,
        model: &dyn RuntimeModel,
        runtime: &LoadedModule,
        stored: &[crate::AddressRange<VirtualAddress>],
        frame: &SampledFrame,
    ) -> Option<(bool, crate::unwind::RegisterFile)> {
        let frame_pointer = frame.frame_pointer?;
        let mapped = self.mapping_at(process, frame.address.get())?;
        let generated = mapped.generated;
        let owned = generated
            || (mapped.loaded.image == runtime.image
                && model.owns_code(mapped.loaded.load_bias.wrapping_unload(frame.address)))
            || stored
                .iter()
                .any(|range| range.start <= frame.address && frame.address < range.end);
        owned.then(|| {
            let registers = crate::unwind::RegisterFile::new([(crate::unwind::RBP, frame_pointer)]);
            (generated, registers)
        })
    }

    /// The layout the BPF walker reads `process`'s interpreter by, if it
    /// runs one microscope reads, or why it cannot be read so.
    pub fn python_layout(&mut self, process: Pid) -> Option<Result<RawPyLayout, String>> {
        self.interpreter(process)?;
        let (python, bias) = self.processes.get(&process)?.iter().find_map(|mapped| {
            self.runtimes.get(&(process, mapped.loaded.id))?.as_ref()?;
            let python = mapped.image.info.as_ref()?.image.python_runtime()?;
            Some((python, mapped.loaded.load_bias))
        })?;

        Some(sampler::process_layout(
            python.layout.as_ref().map_err(AsRef::as_ref),
            bias.load(python.runtime.start),
            process.get(),
        ))
    }

    /// Unwinds a stack the kernel sampled: the registers, a copy of the
    /// stack from the stack pointer up, and the kernel's frame-pointer
    /// walk, instruction first; and the interpreter's frames, from the
    /// record of them the walker sent after it, if any, each segment
    /// before the activation of the loop that runs it.
    pub fn unwind_sampled(
        &mut self,
        process: Pid,
        sample: SampledState<'_>,
        python: Option<&[u8]>,
    ) -> CallStack {
        self.cover(process, sample.chain.first().copied());
        let (mut frames, mut end) = self.unwind_copy(process, sample);
        if self.uncover(process, &frames) {
            (frames, end) = self.unwind_copy(process, sample);
        }
        let runtime = self.interpreter(process);
        let modules = self.modules(process);
        let mut stack = stack(&frames, end);
        if let Some((model, loaded)) = runtime
            && !model.owns_generated_code()
        {
            let activations = activations(model.as_ref(), &modules, &frames);
            let stop = LiveStop::memory(process.get(), loaded.load_bias);
            let thread = crate::ThreadId::new(0);
            let read = match read_record(model.as_ref(), &stop, python) {
                Sampled::Frames { frames, ended } => InterpretedStacks {
                    threads: vec![InterpretedThread {
                        thread,
                        interpreter: 0,
                        frames: 0..frames.len(),
                        end: ended.then(|| "the walker stopped before the outermost frame".into()),
                    }],
                    frames,
                },
                Sampled::None | Sampled::Unknown => InterpretedStacks::default(),
            };
            let (interpreted, truncation) =
                place(&read, thread, &activations, stack.addresses.len());
            stack.interpreted = interpreted.into();
            stack.truncated = stack
                .truncated
                .or_else(|| truncation.map(|reason| reason as u8));
        }

        stack
    }

    /// Unwinds a sampled stack of `process` through its modules.
    fn unwind_copy(
        &mut self,
        process: Pid,
        sample: SampledState<'_>,
    ) -> (Vec<SampledFrame>, StackEnd) {
        let modules: Vec<UnwindModule<'_>> = self
            .processes
            .get(&process)
            .into_iter()
            .flatten()
            .filter_map(|mapped| {
                let info = mapped.image.info.as_deref()?;
                Some(UnwindModule {
                    loaded: mapped.loaded,
                    image: &info.image,
                    unwind: info.unwind.as_ref(),
                })
            })
            .collect();
        unwind_sample(&modules, &mut self.rules, sample, MOST_FRAMES)
    }

    /// Reads `process`'s mappings again when `frames` end at code in no
    /// module the reading knows, as a runtime's that it mapped since:
    /// whether a mapping now holds that code.
    fn uncover(&mut self, process: Pid, frames: &[SampledFrame]) -> bool {
        let Some(last) = frames.last().filter(|frame| frame.module.is_none()) else {
            return false;
        };
        let address = last.address.get();
        if self
            .uncovered
            .get(&process)
            .is_some_and(|addresses| addresses.contains(&address))
        {
            return false;
        }
        self.cover(process, Some(address));
        let covered = self.mapping_at(process, address).is_some();
        if !covered {
            self.uncovered.entry(process).or_default().insert(address);
        }
        covered
    }

    /// Names each address of a stack of `process`'s.
    pub fn frames(&mut self, process: Pid, stack: &[u64]) -> Vec<Frame> {
        if let Some(&first) = stack.first() {
            self.cover(process, Some(first));
        }

        stack
            .iter()
            .enumerate()
            .map(|(index, &address)| self.frame(process, address, index == 0))
            .collect()
    }

    fn frame(&self, process: Pid, address: u64, innermost: bool) -> Frame {
        // A return address may follow a call that never returns, past the
        // end of its function: the call itself names the caller's code.
        let code = if innermost {
            address
        } else {
            address.wrapping_sub(1)
        };

        let mapped = self.mapping_at(process, code);
        let Some(mapped) = mapped else {
            return Frame {
                address: VirtualAddress::new(code),
                module: None,
                symbol: None,
                source: None,
            };
        };

        let code_address = mapped
            .loaded
            .load_bias
            .wrapping_unload(VirtualAddress::new(code));
        let (symbol, source) = mapped.image.info.as_deref().map_or((None, None), |info| {
            let image = &info.image;
            let symbol = image.symbolize(code_address).map(|found| {
                let name = image
                    .symbol(found.symbol)
                    .and_then(crate::Symbol::demangled_name)
                    .map_or(found.name, Arc::from);
                (name, found.offset)
            });
            let source = image.source_location(code_address).and_then(|location| {
                let file = image.source_file(location.file)?;
                Some((file.path.display().to_string(), location.line.get()))
            });
            (symbol, source)
        });

        Frame {
            address: VirtualAddress::new(code),
            module: Some((Arc::clone(&mapped.path), code_address)),
            symbol,
            source,
        }
    }

    fn mapping_at(&self, process: Pid, address: u64) -> Option<Mapped> {
        self.processes
            .get(&process)?
            .iter()
            .find(|mapped| mapped.start <= address && address < mapped.end)
            .cloned()
    }

    /// Records a stack of `process`'s in a capture, each frame by its
    /// module's image address and with its lines, and says which it is.
    pub(crate) fn capture(
        &mut self,
        process: Pid,
        stack: &CallStack,
        builder: &mut CaptureBuilder,
    ) -> u32 {
        if let Some(&first) = stack.addresses.first() {
            self.cover(process, Some(first));
        }

        let mut frames = Vec::with_capacity(stack.addresses.len() + stack.interpreted.len() + 1);
        for frame in stack.frames() {
            let index = match frame {
                Ok(index) => index,
                Err(call) => {
                    frames.push(builder.interpreted(&InterpretedPlace {
                        function: &call.function,
                        file: call.file.as_deref(),
                        line: call.line,
                        declared: call.first_line,
                    }));
                    continue;
                }
            };

            let address = stack.addresses[index];
            let code = if index == 0 {
                address
            } else {
                address.wrapping_sub(1)
            };

            let Some(mapped) = self.mapping_at(process, code) else {
                frames.push(builder.frame(FrameKind::Native, NONE, code, 0));
                continue;
            };

            let image_address = mapped
                .loaded
                .load_bias
                .wrapping_unload(VirtualAddress::new(code))
                .get();
            let module = mapped.image.identity.as_ref().map_or(NONE, |identity| {
                builder.module(
                    &mapped.path,
                    identity.build_id.as_deref(),
                    identity.digest,
                    identity.size,
                )
            });
            let frame = builder.frame(FrameKind::Native, module, image_address, 0);
            if let Some(info) = &mapped.image.info
                && module != NONE
                && !builder.symbolized(frame)
            {
                let lines = symbolize_lines(&info.image, ImageAddress::new(image_address));
                let inputs: Vec<LineInput<'_>> = lines.iter().map(OwnedLine::input).collect();
                builder.symbolize(frame, module, &inputs);
            }

            frames.push(frame);
        }

        if let Some(reason) = stack.truncation() {
            frames.push(builder.truncation(reason, NONE));
        }

        builder.stack(&frames)
    }

    fn modules(&self, process: Pid) -> Vec<UnwindModule<'_>> {
        self.processes
            .get(&process)
            .into_iter()
            .flatten()
            .filter_map(|mapped| {
                let info = mapped.image.info.as_deref()?;
                Some(UnwindModule {
                    loaded: mapped.loaded,
                    image: &info.image,
                    unwind: info.unwind.as_ref(),
                })
            })
            .collect()
    }

    /// A process forked from another, whose mappings it begins with.
    pub fn forked(&mut self, parent: Pid, child: Pid) {
        if let Some(mapped) = self.processes.get(&parent).cloned() {
            self.processes.insert(child, mapped);
        }
    }

    /// Reads what the process maps now from `/proc`.
    pub fn read_maps(&mut self, process: Pid) {
        self.forget(process);
        self.cover(process, None);
    }

    /// Reads the process's modules again unless they map `address`.
    fn cover(&mut self, process: Pid, address: Option<u64>) {
        let known = self.processes.get(&process).is_some_and(|mapped| {
            address.is_none_or(|address| {
                mapped
                    .iter()
                    .any(|mapped| mapped.start <= address && address < mapped.end)
            })
        });
        if known {
            return;
        }

        let Ok(mappings) = perf::mappings(process.get()) else {
            return;
        };

        let mut modules = Vec::new();
        for mapping in &mappings {
            let mmap = decode::Mmap {
                pid: process,
                start: mapping.start,
                length: mapping.end - mapping.start,
                file_offset: mapping.file_offset,
                inode: Some(mapping.inode),
                executable: mapping.executable,
                path: &mapping.path,
                time: 0,
            };

            modules.extend(self.module(&mmap));
        }

        let anonymous: Vec<&perf::Mapped> = mappings
            .iter()
            .filter(|mapping| perf::is_anonymous(mapping))
            .collect();
        self.runtime_code(process, &mut modules, &anonymous);
        self.runtimes.retain(|&(of, _), _| of != process);
        self.processes.insert(process, modules);
    }

    /// Adds to the `modules` of `process` a module for each of its mappings
    /// of `anonymous` code, where a module of them carries V8: a copy of a
    /// module's code, proven byte for byte, binds that module's image, and
    /// any other is code V8 generated, whose frames keep V8's frame-pointer
    /// convention (`debug_info::runtime_code`), as the debugger has them.
    fn runtime_code(
        &mut self,
        process: Pid,
        modules: &mut Vec<Mapped>,
        anonymous: &[&perf::Mapped],
    ) {
        let v8 = modules.iter().any(|mapped| {
            mapped
                .image
                .info
                .as_ref()
                .is_some_and(|info| crate::debug_info::v8::is_v8(&info.image))
        });
        if !v8 || anonymous.is_empty() {
            return;
        }

        let sources: Vec<crate::unwind::code_copies::Source<'_, usize>> = modules
            .iter()
            .enumerate()
            .filter_map(|(index, mapped)| {
                let info = mapped.image.info.as_ref()?;
                Some(crate::unwind::code_copies::Source {
                    key: index,
                    image: &info.image,
                    loaded: mapped.loaded,
                    stored: stored_code(process, mapped),
                })
            })
            .collect();
        let found: Vec<_> = anonymous
            .iter()
            .map(|mapping| {
                let range = mapping.start..mapping.end;
                let copy = (!mapping.writable)
                    .then(|| perf::copy_of(process.get(), &range, &sources))
                    .flatten()
                    .map(|proven| (proven.source, proven.image_start, proven.length));
                (range, copy)
            })
            .collect();
        drop(sources);

        for (range, copy) in found {
            let id = ModuleId::new(self.next_module);
            self.next_module += 1;
            let mapped = if let Some((source, image_start, length)) = copy {
                let source = &modules[source];
                let image_end = ImageAddress::new(image_start.get().saturating_add(length));
                Mapped {
                    start: range.start,
                    end: range.start.saturating_add(length),
                    path: Arc::clone(&source.path),
                    loaded: LoadedModule {
                        id,
                        image: source.loaded.image,
                        load_bias: LoadBias::between(image_start, VirtualAddress::new(range.start)),
                        window: Some(crate::AddressRange {
                            start: image_start,
                            end: image_end,
                        }),
                    },
                    image: source.image.clone(),
                    generated: false,
                }
            } else {
                let path = format!("[V8 code {:#x}]", range.start);
                let image_id = self.next_image_id();
                let bounds = crate::AddressRange {
                    start: ImageAddress::new(range.start),
                    end: ImageAddress::new(range.end),
                };
                let Ok(info) = crate::debug_info::runtime_code::load(
                    std::path::Path::new(&path),
                    image_id,
                    bounds,
                ) else {
                    continue;
                };
                Mapped {
                    start: range.start,
                    end: range.end,
                    path: path.into(),
                    loaded: LoadedModule {
                        id,
                        image: image_id,
                        load_bias: LoadBias::new(0),
                        window: None,
                    },
                    image: Image {
                        info: Some(Arc::new(info)),
                        identity: None,
                    },
                    generated: true,
                }
            };
            modules.push(mapped);
        }
    }

    const fn next_image_id(&mut self) -> ModuleImageId {
        let id = ModuleImageId::new(self.next_image);
        self.next_image += 1;
        id
    }

    fn image(&mut self, file: &std::path::Path, inode: u64) -> Image {
        let key = (file.to_path_buf(), inode);
        if let Some(image) = self.images.get(&key) {
            return image.clone();
        }

        let id = self.next_image_id();
        let image = crate::image::backing::read_input(file)
            .ok()
            .map(|(data, _)| Image {
                info: crate::debug_info::load_module_bytes(file, &data, id, &self.search)
                    .ok()
                    .map(Arc::new),
                identity: Some(Arc::new(Identity::of(&data))),
            })
            .unwrap_or_default();
        self.images.insert(key, image.clone());
        image
    }
}

/// The ranges of `mapped`'s image that a runtime it carries says process
/// `process` keeps its own code in, where no symbol bounds it, as V8's
/// builtins in a stripped libnode, where a copy of them may begin.
fn stored_code(process: Pid, mapped: &Mapped) -> Vec<(ImageAddress, ImageAddress)> {
    let Some(info) = &mapped.image.info else {
        return Vec::new();
    };
    let image: Arc<dyn RuntimeImage + Send + Sync> = Arc::clone(&info.image) as _;
    if image.v8().is_none() {
        return Vec::new();
    }
    let stop = LiveStop::memory(process.get(), mapped.loaded.load_bias);
    runtime_model::detect(&image)
        .into_iter()
        .flatten()
        .flat_map(|model| model.process_code(&stop))
        .filter(|range| mapped.start <= range.start.get() && range.end.get() <= mapped.end)
        .map(|range| {
            let bias = mapped.loaded.load_bias;
            (
                bias.wrapping_unload(range.start),
                bias.wrapping_unload(range.end),
            )
        })
        .collect()
}

/// The stack of unwound frames: the innermost frame's instruction, and for
/// each frame after, the return address its call left, unless a signal
/// interrupted it at the instruction itself.
fn stack(frames: &[SampledFrame], end: StackEnd) -> CallStack {
    let addresses = frames
        .iter()
        .enumerate()
        .map(|(index, frame)| {
            let interrupted = index == 0
                || frame.role == CodeRole::SignalTrampoline
                || frames[index - 1].role == CodeRole::SignalTrampoline;
            if interrupted {
                frame.address.get()
            } else {
                frame.address.get().wrapping_add(1)
            }
        })
        .collect();
    CallStack {
        addresses,
        truncated: match end {
            StackEnd::Complete => None,
            StackEnd::Truncated(reason, _) => Some(reason as u8),
        },
        interpreted: Arc::default(),
    }
}

/// The indexes of the `frames` unwound through `modules` that are
/// activations of `model`'s interpreter's loop, by the symbols they run.
fn activations(
    model: &dyn RuntimeModel,
    modules: &[UnwindModule<'_>],
    frames: &[SampledFrame],
) -> Vec<usize> {
    // Only the interpreter's own code can be its loop, or call from it.
    let names: Vec<Option<Arc<str>>> = frames
        .iter()
        .map(|frame| {
            let module = modules.get(frame.module?)?;
            (frame.role == CodeRole::Interpreter).then_some(())?;
            let address = module.loaded.load_bias.wrapping_unload(frame.address);
            Some(module.image.symbolize(address)?.name)
        })
        .collect();

    (0..frames.len())
        .filter(|&index| {
            let inner = index
                .checked_sub(1)
                .and_then(|inner| names[inner].as_deref());
            names[index]
                .as_deref()
                .is_some_and(|name| model.activation(name, inner))
        })
        .collect()
}

/// The frames `stacks` names on `thread`, each segment an activation of
/// the interpreter's loop runs just before that activation, the innermost
/// before the innermost, where `activations` are the indexes of the
/// native frames that are ones, innermost first; and where the frames say
/// the stack ends short. Where the activations and segments differ in
/// number, no segment's place is known, and the frames follow all
/// `natives` native frames, as the debugger shows them
/// (`backend::linux::interpreted`).
fn place(
    stacks: &InterpretedStacks,
    thread: crate::ThreadId,
    activations: &[usize],
    natives: usize,
) -> (Vec<InterpretedCall>, Option<Truncation>) {
    let call = |frame, before: usize| {
        let wait = InterpretedWait::of_frame(frame);
        InterpretedCall {
            before: u32::try_from(before).unwrap_or(u32::MAX),
            function: wait.function,
            file: wait.file,
            line: wait.line,
            first_line: wait.first_line,
        }
    };

    let threads: Vec<_> = stacks
        .threads
        .iter()
        .filter(|interpreted| interpreted.thread == thread)
        .collect();
    let [interpreted] = threads.as_slice() else {
        let frames = threads
            .iter()
            .flat_map(|interpreted| stacks.frames_of(interpreted))
            .map(|frame| call(frame, natives))
            .collect::<Vec<_>>();
        // A thread in the loop runs frames, which could not be read, and
        // the frames of several states have no known order.
        let truncation = if frames.is_empty() {
            (!activations.is_empty()).then_some(Truncation::InterpretedUnknown)
        } else {
            Some(Truncation::InterpretedUnplaced)
        };

        return (frames, truncation);
    };

    let frames = stacks.frames_of(interpreted);
    let segments: Vec<_> = frames.split_inclusive(|frame| frame.outermost).collect();
    if segments.len() != activations.len() {
        let frames = frames.iter().map(|frame| call(frame, natives)).collect();
        return (frames, Some(Truncation::InterpretedUnplaced));
    }

    let placed = activations
        .iter()
        .zip(segments)
        .flat_map(|(&activation, segment)| segment.iter().map(move |frame| call(frame, activation)))
        .collect();

    (
        placed,
        interpreted
            .end
            .is_some()
            .then_some(Truncation::InterpretedEnded),
    )
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    /// Interpreted frames named by their functions, each `true` the
    /// outermost of its segment.
    fn interpreted(thread: u64, frames: &[(&str, bool)]) -> InterpretedStacks {
        InterpretedStacks {
            threads: vec![runtime_model::InterpretedThread {
                thread: crate::ThreadId::new(thread),
                interpreter: 0,
                frames: 0..frames.len(),
                end: None,
            }],
            frames: frames
                .iter()
                .map(|&(function, outermost)| runtime_model::InterpretedFrame {
                    frame: VirtualAddress::new(0x1000),
                    language: crate::SourceLanguage::Python,
                    function: Some(function.into()),
                    place: Some(crate::RecordedPlace {
                        path: "main.py".into(),
                        line: 3,
                        column: 0,
                    }),
                    problem: None,
                    first_line: Some(1),
                    outermost,
                    role: CodeRole::Ordinary,
                })
                .collect(),
        }
    }

    fn placed(placed: &(Vec<InterpretedCall>, Option<Truncation>)) -> Vec<(&str, u32)> {
        placed
            .0
            .iter()
            .map(|call| (call.function.as_ref(), call.before))
            .collect()
    }

    /// Each segment goes before its activation, innermost with innermost.
    /// Where the two differ in number, or the thread runs several states,
    /// the frames follow the native ones and the stack says why; a thread
    /// in the loop whose frames were not read says that.
    #[test]
    fn interpreted_frames_go_before_their_activations_or_say_why_not() {
        let thread = crate::ThreadId::new(5);
        let stacks = interpreted(5, &[("inner", false), ("middle", true), ("outer", true)]);
        let found = place(&stacks, thread, &[2, 6], 9);
        assert_eq!(placed(&found), [("inner", 2), ("middle", 2), ("outer", 6)]);
        assert_eq!(found.1, None);

        let found = place(&stacks, thread, &[2], 9);
        assert_eq!(placed(&found), [("inner", 9), ("middle", 9), ("outer", 9)]);
        assert_eq!(found.1, Some(Truncation::InterpretedUnplaced));

        let mut ended = stacks.clone();
        ended.threads[0].end = Some("unreadable".into());
        assert_eq!(
            place(&ended, thread, &[2, 6], 9).1,
            Some(Truncation::InterpretedEnded)
        );

        let mut doubled = stacks.clone();
        doubled.threads.push(doubled.threads[0].clone());
        let found = place(&doubled, thread, &[2, 6], 9);
        assert_eq!(found.0.len(), 6);
        assert_eq!(found.1, Some(Truncation::InterpretedUnplaced));

        let other = crate::ThreadId::new(6);
        assert_eq!(
            place(&stacks, other, &[2], 9),
            (Vec::new(), Some(Truncation::InterpretedUnknown))
        );
        assert_eq!(place(&stacks, other, &[], 9), (Vec::new(), None));
    }

    #[test]
    fn the_vdso_and_a_file_read_after_it_have_their_own_images() {
        let mut stacks = Stacks::default();
        let (vdso, _) = stacks.vdso().expect("microscope's own vDSO is readable");
        let file = Path::new(env!("CARGO_MANIFEST_DIR")).join("build/test-programs/basic");
        let file = stacks.image(&file, 0);
        let id = |image: &Image| image.info.as_ref().map(|info| info.image.id());
        assert!(id(&vdso).is_some() && id(&file).is_some());
        assert_ne!(id(&vdso), id(&file));
    }
}
