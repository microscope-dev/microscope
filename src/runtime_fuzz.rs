//! The fuzz harness for the runtime models: every question a session asks
//! a runtime at a stop, over memory the input writes.
//!
//! Each target loads one real image once: a Go program, a tokio program,
//! CPython, or Node. Memory is the image's own file, as the loader maps
//! it, with a heap and each thread's thread-local block mapped as zeros.
//! The input writes into it relative to anchors: the heap's regions, each
//! thread's thread pointer, and every symbol and thread-local the models
//! looked up while they bound and answered an empty stop. So a pointer the
//! input writes can name anything the models read first, or itself, as
//! corrupt memory in a core does.
//!
//! The input's first byte shapes the stop: how many threads it has,
//! whether it keeps a [`StopMemory`], and whether the program runs. Its
//! first eight bytes also choose what is asked, such as a page's size.
//! Past them, each command writes bytes, a word, a run of one byte, or a
//! field the models read at an anchor, sets a thread's register, futex,
//! or target id, or names a task set. A word is an anchor and an offset,
//! a constant the models compare with, or eight bytes as they are.
#![expect(
    clippy::indexing_slicing,
    reason = "a fuzz harness reports a bug by panicking"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use crate::inspection::InspectionBudget;
use crate::runtime_model::{
    Arming, CodeAt, CodeFrame, DrivenFuture, HookPlace, ImageSymbol, InterpretedChild,
    InterpretedFrameRef, InterpretedHandle, InterpretedKey, InterpretedName, InterpretedSlot,
    InterpretedStep, LiveThread, Member, RuntimeImage, RuntimeModel, RuntimeStop, SampledCode,
    SampledCodeFrame, SampledFrame, StopMemory, StoredValue, TargetThreadId, TaskContext, TaskRef,
    futures,
};
use crate::unwind::{DwarfRegister, FS_BASE, RETURN_ADDRESS, RSP, RegisterFile};
use crate::{
    CoroutineInfo, ImageAddress, InspectionLimits, IntegerValue, ModuleImage, ThreadId,
    ThreadLocal, TypeInfo, TypeNode, TypeReference, VirtualAddress,
};

/// The image a target loads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Runtime {
    Go,
    Tokio,
    Python,
    Node,
}

/// Where the heap is, and how large: regions of [`REGION`] bytes, each an
/// anchor.
const HEAP: u64 = 0x7000_0000_0000;
const REGION: u64 = 0x1_0000;
const REGIONS: u64 = 32;

/// Each thread's thread pointer is in the middle of a block of
/// [`THREAD_BLOCK`] bytes, the first at [`THREADS`]; the C library's block
/// for the module's own thread-locals is [`DYNAMIC`] into it.
const THREADS: u64 = 0x7100_0000_0000;
const THREAD_BLOCK: u64 = 0x10_0000;
const DYNAMIC: u64 = 0x1000;

/// The most threads the input stops.
const MOST_THREADS: u8 = 4;

/// The most anchors kept, so that one byte names any, and the most of
/// them in code.
const MOST_ANCHORS: usize = 255;
const MOST_CODE_ANCHORS: usize = 64;

/// The most names noted, and the most fields and constants kept.
const MOST_LOOKED_UP: usize = 4096;

const PAGE: u64 = 4096;

/// The most pages one run writes, which bounds its memory.
const MOST_PAGES: usize = 1024;

/// The most of each kind of thing asked of a model at one stop, so that
/// a run stays short.
const MOST_TASKS: usize = 64;
const MOST_FRAMES: usize = 32;
const MOST_CHILDREN: u64 = 16;
const MOST_LOCALS: usize = 16;
const MOST_FUTURES: usize = 64;
const MOST_SEARCHED: usize = 4;

/// The most pages of tasks listed. A runtime's own bounds allow far more,
/// as Go's 16M goroutines do, which a run need not read through.
const MOST_PAGES_LISTED: usize = 16;

/// The names an interpreted frame is asked for.
const NAMES: [&str; 8] = [
    "x", "self", "value", "__name__", "len", "Object", "this", "a",
];

pub fn fuzz(runtime: Runtime, data: &[u8]) {
    let fixture = Fixture::get(runtime);
    let stop = Stop::new(fixture, data);
    let query = Query::new(data);
    for model in &fixture.models {
        drive(fixture, model.as_ref(), &stop, &query);
    }

    for (index, &ty) in fixture.futures.iter().enumerate() {
        let future = stop.anchor(query.byte(index), u64::from(query.byte(index + 1)) * 8);
        let chain = futures::walk(fixture.image.as_ref(), &stop, future, ty);
        debug_assert!(chain.frames.len() <= futures::MAX_DEPTH);
    }
}

/// What a target binds once: its image, the models it carries, and the
/// anchors the input writes relative to.
struct Fixture {
    image: Arc<dyn RuntimeImage + Send + Sync>,
    models: Vec<Arc<dyn RuntimeModel>>,

    /// The image's loaded segments: address, size, and the file's bytes
    /// for them, which may be fewer.
    segments: Vec<(u64, u64, Arc<[u8]>)>,

    /// Where the input writes relative to, by index.
    anchors: Vec<u64>,

    /// The fields the models read, each its offset within its object and
    /// its size, and the constants they compare with, by index.
    fields: Vec<(u64, u64)>,
    constants: Vec<u64>,

    /// The futures the image's types describe, which are walked at
    /// anchors.
    futures: Vec<TypeReference>,
}

impl Fixture {
    fn get(runtime: Runtime) -> &'static Self {
        static FIXTURES: [OnceLock<Fixture>; 4] = [const { OnceLock::new() }; 4];
        let index = match runtime {
            Runtime::Go => 0,
            Runtime::Tokio => 1,
            Runtime::Python => 2,
            Runtime::Node => 3,
        };
        FIXTURES[index].get_or_init(|| Self::load(runtime))
    }

    #[expect(
        clippy::panic,
        reason = "a fuzz target without its image tests nothing"
    )]
    fn load(runtime: Runtime) -> Self {
        let path = image_path(runtime);
        let module = crate::debug_info::load_module(
            &path,
            crate::ModuleImageId::new(0),
            &crate::debug_info::DebugFileSearch::default(),
        )
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
        .image;
        let segments = segments(&path);
        let futures = future_types(&module);
        let recording = Arc::new(Recording {
            module,
            looked_up: Mutex::default(),
        });
        let image: Arc<dyn RuntimeImage + Send + Sync> = recording.clone();
        let models: Vec<_> = crate::runtime_model::detect(&image)
            .into_iter()
            .filter_map(Result::ok)
            .collect();
        assert!(!models.is_empty(), "{} binds no runtime", path.display());

        let mut fixture = Self {
            image,
            models,
            segments,
            anchors: Vec::new(),
            fields: Vec::new(),
            constants: Vec::new(),
            futures,
        };

        // What the models look up while they answer an empty stop is what
        // they read first, as a session's first questions would.
        let empty = Stop::new(&fixture, &[(MOST_THREADS - 1) << 3]);
        for model in &fixture.models {
            drive(&fixture, model.as_ref(), &empty, &Query::new(&[]));
        }

        fixture.anchors = anchors(&recording, &fixture.models, &empty);
        (fixture.fields, fixture.constants) = fields(&recording);
        fixture
    }
}

#[expect(
    clippy::panic,
    reason = "a fuzz target without its image tests nothing"
)]
fn image_path(runtime: Runtime) -> PathBuf {
    let programs = Path::new(env!("CARGO_MANIFEST_DIR")).join("build/test-programs");
    let shell = |variable: &str| {
        PathBuf::from(std::env::var_os(variable).unwrap_or_else(|| {
            panic!("{variable} names the dev shell's runtimes; run in the dev shell")
        }))
    };

    match runtime {
        Runtime::Go => programs.join("steps-go-o2"),
        Runtime::Tokio => programs.join("tokio-workers-o0"),
        Runtime::Python => python_module(&shell("MICROSCOPE_CPYTHON").join("3.14")),
        Runtime::Node => shell("MICROSCOPE_NODE").join("26/node"),
    }
}

/// The file that carries CPython's runtime: its executable, or the
/// library the executable links.
#[expect(
    clippy::panic,
    reason = "a fuzz target without its image tests nothing"
)]
fn python_module(directory: &Path) -> PathBuf {
    let executable = std::fs::canonicalize(directory.join("python"))
        .unwrap_or_else(|error| panic!("{}: {error}", directory.display()));
    let lib = executable
        .parent()
        .and_then(Path::parent)
        .map(|prefix| prefix.join("lib"));
    lib.and_then(|lib| std::fs::read_dir(lib).ok())
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("libpython") && name.ends_with(".so.1.0"))
        })
        .unwrap_or(executable)
}

/// The loaded segments of the file at `path`.
#[expect(
    clippy::panic,
    reason = "a fuzz target without its image tests nothing"
)]
fn segments(path: &Path) -> Vec<(u64, u64, Arc<[u8]>)> {
    use object::{Object, ObjectSegment};

    let bytes = std::fs::read(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    let file = object::File::parse(bytes.as_slice())
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    file.segments()
        .filter(|segment| segment.size() > 0)
        .map(|segment| {
            let data = segment.data().unwrap_or_default();
            (segment.address(), segment.size(), Arc::from(data))
        })
        .collect()
}

/// The image's coroutines, and the pointers, pins, and trait objects of
/// futures that may hold one, in identifier order.
fn future_types(module: &ModuleImage) -> Vec<TypeReference> {
    module
        .types()
        .filter_map(|node| match node {
            TypeNode::Resolved(info) => Some(info),
            TypeNode::Malformed { .. } => None,
        })
        .filter(|info| {
            module.coroutine(info.reference.id).is_some()
                || (info.name.contains("Future")
                    && ["Pin<", "Box<", "&", "*"]
                        .iter()
                        .any(|start| info.name.starts_with(start)))
        })
        .map(|info| info.reference)
        .take(MOST_FUTURES)
        .collect()
}

/// The anchors: the heap's regions, each thread's thread pointer, where
/// each thread keeps each thread-local the models looked up, the symbols
/// they looked up, the Python and V8 runtimes' places, and the code the
/// models trap in.
fn anchors(recording: &Recording, models: &[Arc<dyn RuntimeModel>], stop: &Stop<'_>) -> Vec<u64> {
    let mut anchors: Vec<u64> = (0..REGIONS).map(|region| HEAP + region * REGION).collect();
    let threads = stop.threads();
    anchors.extend(threads.iter().map(|&thread| {
        stop.thread_pointer(thread)
            .map_or(0, |pointer| pointer - REGION / 2)
    }));

    let looked_up = recording
        .looked_up
        .lock()
        .map(|looked_up| looked_up.clone())
        .unwrap_or_default();
    let image = recording.module.as_ref();
    let mut places = BTreeSet::new();
    for name in &looked_up {
        match name {
            LookedUp::Symbol(name) => {
                if let Some(symbol) = RuntimeImage::symbol(image, name) {
                    places.insert(symbol.address.get());
                }
            }
            LookedUp::Function(name) => {
                if let Some(symbol) = RuntimeImage::function_answering(image, name) {
                    places.insert(symbol.address.get());
                }
            }
            LookedUp::Member(..) | LookedUp::Constant(_) | LookedUp::Type(_) => {}
            LookedUp::ThreadLocal(scope, name) => {
                let place = scope.as_ref().map_or_else(
                    || RuntimeImage::thread_local(image, name),
                    |scope| RuntimeImage::thread_local_within(image, scope, name),
                );
                if let Some(Ok(place)) = place {
                    for &thread in &threads {
                        if let Ok(address) = stop.thread_local_address(thread, place) {
                            anchors.push(address.get());
                        }
                    }
                }
            }
        }
    }

    if let Some(python) = image.python_runtime() {
        places.insert(python.runtime.start.get());
    }

    if let Some(v8) = image.v8_runtime() {
        if let Ok(blob) = v8.embedded_blob {
            places.extend(
                [blob.code, blob.code_size, blob.data, blob.data_size].map(ImageAddress::get),
            );
        }
        if let Ok(Some(crate::image::v8::DispatchTable::Group { group, .. })) = v8.dispatch_table {
            places.insert(group.get());
        }
        if let Ok(tasks) = v8.tasks {
            places.insert(tasks.context_tag.get());
        }
    }

    anchors.extend(places);

    // Code goes last: registers name it, and few writes do.
    let mut code: Vec<u64> = models
        .iter()
        .flat_map(|model| code_places(model.as_ref()))
        .map(ImageAddress::get)
        .collect();
    code.sort_unstable();
    code.dedup();
    anchors.extend(code.into_iter().take(MOST_CODE_ANCHORS));
    anchors.truncate(MOST_ANCHORS);
    anchors
}

/// The fields the models read: the members they looked up, those of the
/// types they looked at, and the fields of the Python and V8 layouts; and
/// the constants they compare with: those they looked up, a sum type's
/// tags, and the layouts' own.
fn fields(recording: &Recording) -> (Vec<(u64, u64)>, Vec<u64>) {
    let looked_up = recording
        .looked_up
        .lock()
        .map(|looked_up| looked_up.clone())
        .unwrap_or_default();
    let image = recording.module.as_ref();
    let mut fields = BTreeSet::new();
    let mut constants: BTreeSet<u64> = (0..8).collect();
    for name in &looked_up {
        match name {
            LookedUp::Member(ty, path) => {
                let path: Vec<&str> = path.iter().map(String::as_str).collect();
                if let Some(member) = RuntimeImage::member(image, ty, &path) {
                    fields.insert((member.offset, member.size));
                }
            }
            LookedUp::Constant(name) => {
                if let Some(value) = RuntimeImage::constant(image, name) {
                    constants.insert(low_word(value));
                }
            }
            LookedUp::Type(ty) => {
                if let Some(info) = image.type_info(*ty) {
                    type_fields(image, info, &mut fields, &mut constants);
                }
            }
            LookedUp::Symbol(_) | LookedUp::Function(_) | LookedUp::ThreadLocal(..) => {}
        }
    }

    if let Some(Ok(layout)) = image.python_runtime().map(|runtime| runtime.layout) {
        use crate::image::python::{Constant, Field};
        fields.extend(
            Field::ALL
                .iter()
                .filter_map(|&field| layout.field(field))
                .map(|at| (at, 8)),
        );
        constants.extend(
            Constant::ALL
                .iter()
                .map(|&constant| u64::from(layout.constant(constant))),
        );
    }

    if let Some(Ok(layout)) = image.v8_runtime().map(|runtime| runtime.layout) {
        use crate::image::v8::{Constant, Field};
        fields.extend(
            Field::ALL
                .iter()
                .filter_map(|&field| layout.field(field))
                .map(|at| (at, 8)),
        );
        constants.extend(
            Constant::ALL
                .iter()
                .filter_map(|&constant| layout.constant(constant))
                .map(i64::cast_unsigned),
        );
    }

    let mut fields: Vec<_> = fields.into_iter().collect();
    let mut constants: Vec<_> = constants.into_iter().collect();
    fields.truncate(MOST_LOOKED_UP);
    constants.truncate(MOST_LOOKED_UP);
    (fields, constants)
}

/// A constant's low word, which is what memory holds of it.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a word's worth of the constant"
)]
const fn low_word(value: IntegerValue) -> u64 {
    match value {
        IntegerValue::Signed(value) => value as u64,
        IntegerValue::Unsigned(value) => value as u64,
    }
}

/// The size of a value of `ty`, or a word where it is unknown.
fn byte_size(image: &ModuleImage, ty: TypeReference) -> u64 {
    image
        .type_info(ty)
        .and_then(|info| crate::runtime_model::representation(image, info))
        .and_then(|info| info.byte_size)
        .unwrap_or(8)
}

/// The fields of the record or sum type `info` describes, and its
/// variants' tags.
fn type_fields(
    image: &ModuleImage,
    info: &TypeInfo,
    fields: &mut BTreeSet<(u64, u64)>,
    constants: &mut BTreeSet<u64>,
) {
    let mut members: Vec<&crate::RecordMember> = Vec::new();
    match &info.kind {
        crate::TypeKind::Record { members: all, .. } => members.extend(all.iter()),
        crate::TypeKind::Variant {
            common_members,
            discriminant,
            variants,
            ..
        } => {
            members.extend(common_members.iter());
            if let crate::VariantDiscriminant::Stored(tag) = discriminant.as_ref() {
                members.push(tag);
            }
            for variant in variants.iter() {
                members.extend(variant.members.iter());
                if let crate::VariantSelection::Selectors(selectors) = &variant.selection {
                    for selector in selectors.iter() {
                        if let crate::VariantSelector::Value(value) = selector {
                            constants.insert(low_word(*value));
                        }
                    }
                }
            }
        }
        _ => {}
    }
    for member in members {
        if let crate::RecordMemberLayout::ByteOffset(at) = member.layout {
            fields.insert((at, byte_size(image, member.type_ref)));
        }
    }
}

/// The code a model traps or describes threads in.
fn code_places(model: &dyn RuntimeModel) -> Vec<ImageAddress> {
    let mut places: Vec<ImageAddress> = model.hooks().iter().map(|hook| hook.address).collect();
    places.extend(model.internal_hooks().iter().filter_map(|hook| match hook {
        HookPlace::Own(at) => Some(*at),
        HookPlace::Companion(_) => None,
    }));
    places.extend(model.task_starters().iter().map(|starter| starter.entry));
    places.extend(
        model
            .frame_replacers()
            .iter()
            .map(|replacer| replacer.entry),
    );
    places.extend(
        [
            model.stack_mover(),
            model.inspector_hook(),
            model.native_entry(),
            model.collector(),
            model.inspector_signal_handler(),
            model.inspector_message_received(),
        ]
        .into_iter()
        .flatten(),
    );
    if let Some(close) = model.inspector_close_functions() {
        places.extend(<[ImageAddress; 2]>::from(close));
    }
    if let Some(hooks) = model.inspector_startup_hooks() {
        places.extend(hooks);
    }
    places.extend(model.frame_pointer_code().iter().map(|range| range.start));
    places.extend(
        model
            .stack_frame_builders()
            .unwrap_or_default()
            .iter()
            .map(|range| range.start),
    );
    places.sort_unstable();
    places.dedup();
    places
}

/// A name a model looked up in the image.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum LookedUp {
    Symbol(String),
    Function(String),
    ThreadLocal(Option<String>, String),
    Member(String, Vec<String>),
    Constant(String),
    Type(TypeReference),
}

/// The image, which notes the names the models look up in it.
#[derive(Debug)]
struct Recording {
    module: Arc<ModuleImage>,
    looked_up: Mutex<BTreeSet<LookedUp>>,
}

impl Recording {
    fn note(&self, name: LookedUp) {
        if let Ok(mut looked_up) = self.looked_up.lock()
            && looked_up.len() < MOST_LOOKED_UP
        {
            looked_up.insert(name);
        }
    }
}

impl RuntimeImage for Recording {
    fn producers(&self) -> Vec<&str> {
        RuntimeImage::producers(self.module.as_ref())
    }

    fn constant(&self, name: &str) -> Option<IntegerValue> {
        self.note(LookedUp::Constant(name.to_owned()));
        RuntimeImage::constant(self.module.as_ref(), name)
    }

    fn symbol(&self, name: &str) -> Option<ImageSymbol> {
        self.note(LookedUp::Symbol(name.to_owned()));
        RuntimeImage::symbol(self.module.as_ref(), name)
    }

    fn functions_starting(&self, prefix: &str) -> Vec<ImageSymbol> {
        RuntimeImage::functions_starting(self.module.as_ref(), prefix)
    }

    fn function_answering(&self, name: &str) -> Option<ImageSymbol> {
        self.note(LookedUp::Function(name.to_owned()));
        RuntimeImage::function_answering(self.module.as_ref(), name)
    }

    fn symbol_at(&self, address: ImageAddress) -> Option<Arc<str>> {
        RuntimeImage::symbol_at(self.module.as_ref(), address)
    }

    fn vtable_class(&self, address: ImageAddress) -> Option<(String, ImageAddress)> {
        RuntimeImage::vtable_class(self.module.as_ref(), address)
    }

    fn has_function(&self, name: &str) -> bool {
        RuntimeImage::has_function(self.module.as_ref(), name)
    }

    fn function_body(&self, name: &str) -> Option<ImageAddress> {
        RuntimeImage::function_body(self.module.as_ref(), name)
    }

    fn function_entries(&self, name: &str) -> Vec<ImageAddress> {
        RuntimeImage::function_entries(self.module.as_ref(), name)
    }

    fn member(&self, type_name: &str, path: &[&str]) -> Option<Member> {
        self.note(LookedUp::Member(
            type_name.to_owned(),
            path.iter().map(|&step| step.to_owned()).collect(),
        ));
        RuntimeImage::member(self.module.as_ref(), type_name, path)
    }

    fn function_name(&self, address: ImageAddress) -> Option<Arc<str>> {
        RuntimeImage::function_name(self.module.as_ref(), address)
    }

    fn function_generics(&self, address: ImageAddress) -> Vec<(Arc<str>, String)> {
        RuntimeImage::function_generics(self.module.as_ref(), address)
    }

    fn thread_local(&self, name: &str) -> Option<Result<ThreadLocal, Arc<str>>> {
        self.note(LookedUp::ThreadLocal(None, name.to_owned()));
        RuntimeImage::thread_local(self.module.as_ref(), name)
    }

    fn thread_local_within(
        &self,
        scope: &str,
        name: &str,
    ) -> Option<Result<ThreadLocal, Arc<str>>> {
        self.note(LookedUp::ThreadLocal(
            Some(scope.to_owned()),
            name.to_owned(),
        ));
        RuntimeImage::thread_local_within(self.module.as_ref(), scope, name)
    }

    fn types_named(&self, name: &str) -> Vec<TypeReference> {
        RuntimeImage::types_named(self.module.as_ref(), name)
    }

    fn type_info(&self, ty: TypeReference) -> Option<&TypeInfo> {
        self.note(LookedUp::Type(ty));
        RuntimeImage::type_info(self.module.as_ref(), ty)
    }

    fn same_type(&self, left: TypeReference, right: TypeReference) -> bool {
        RuntimeImage::same_type(self.module.as_ref(), left, right)
    }

    fn source_path_ending(&self, suffix: &str) -> Option<PathBuf> {
        RuntimeImage::source_path_ending(self.module.as_ref(), suffix)
    }

    fn coroutine(&self, ty: TypeReference) -> Option<Result<&CoroutineInfo, &Arc<str>>> {
        RuntimeImage::coroutine(self.module.as_ref(), ty)
    }

    fn trait_object_type(&self, address: ImageAddress) -> Option<TypeReference> {
        RuntimeImage::trait_object_type(self.module.as_ref(), address)
    }

    fn coroutine_body(&self, ty: TypeReference) -> Option<ImageAddress> {
        RuntimeImage::coroutine_body(self.module.as_ref(), ty)
    }

    fn python(&self) -> Option<crate::image::python::PythonRuntime> {
        RuntimeImage::python(self.module.as_ref())
    }

    fn v8(&self) -> Option<crate::image::v8::V8Runtime> {
        RuntimeImage::v8(self.module.as_ref())
    }
}

/// The input, read a byte at a time, as zeros past its end.
struct Input<'a>(&'a [u8]);

impl Input<'_> {
    const fn byte(&mut self) -> u8 {
        let Some((&byte, rest)) = self.0.split_first() else {
            return 0;
        };
        self.0 = rest;
        byte
    }

    const fn u16(&mut self) -> u16 {
        u16::from_le_bytes([self.byte(), self.byte()])
    }

    fn u64(&mut self) -> u64 {
        u64::from_le_bytes(std::array::from_fn(|_| self.byte()))
    }

    fn bytes(&mut self, count: usize) -> &[u8] {
        let (taken, rest) = self.0.split_at(count.min(self.0.len()));
        self.0 = rest;
        taken
    }
}

/// A stopped thread: its thread pointer, its registers, and what the
/// stop says of it.
struct Thread {
    id: ThreadId,
    pointer: u64,
    registers: RegisterFile,
    futex: Option<u64>,
    target: Option<TargetThreadId>,
}

/// The stop the input describes: memory, threads, and how it is read.
struct Stop<'a> {
    fixture: &'a Fixture,

    /// The pages the input wrote, each whole.
    pages: BTreeMap<u64, Box<[u8]>>,
    threads: Vec<Thread>,
    memory: Option<StopMemory>,
    runs: bool,
    task_sets: Vec<u64>,
}

impl<'a> Stop<'a> {
    /// The stop `data` writes: its first byte the stop's shape, then
    /// commands, each a byte and its operands.
    fn new(fixture: &'a Fixture, data: &[u8]) -> Self {
        let mut input = Input(data.get(Query::BYTES..).unwrap_or_default());
        let shape = data.first().copied().unwrap_or(0);
        let threads = (0..=(shape >> 3) % MOST_THREADS)
            .map(|index| Thread {
                id: ThreadId::new(1000 + u64::from(index)),
                pointer: THREADS + u64::from(index) * THREAD_BLOCK + THREAD_BLOCK / 2,
                registers: RegisterFile::new([]),
                futex: None,
                target: TargetThreadId::new(1000 + u64::from(index)),
            })
            .collect();
        let mut stop = Self {
            fixture,
            pages: BTreeMap::new(),
            threads,
            memory: (shape & 1 != 0).then(StopMemory::default),
            runs: shape & 2 != 0,
            task_sets: Vec::new(),
        };

        while !input.0.is_empty() {
            match input.byte() % 7 {
                0 => {
                    let at = stop.anchor(input.byte(), u64::from(input.u16()));
                    let count = usize::from(input.byte());
                    let bytes = input.bytes(count).to_vec();
                    stop.write(at.get(), &bytes);
                }
                1 => {
                    let at = stop.anchor(input.byte(), u64::from(input.u16()));
                    let value = stop.value(&mut input);
                    stop.write(at.get(), &value.to_le_bytes());
                }
                2 => {
                    let at = stop.anchor(input.byte(), u64::from(input.u16()));
                    let count = usize::from(input.u16() % 4096);
                    let byte = input.byte();
                    stop.write(at.get(), &vec![byte; count]);
                }
                3 => {
                    let thread = usize::from(input.byte()) % stop.threads.len();
                    let register = match input.byte() % 18 {
                        17 => FS_BASE,
                        number => DwarfRegister::new(u16::from(number)),
                    };
                    let value = stop.value(&mut input);
                    if let Some(thread) = stop.threads.get_mut(thread) {
                        thread.registers.set(register, value);
                    }
                }
                4 => {
                    let thread = usize::from(input.byte()) % stop.threads.len();
                    let futex = stop.value(&mut input);
                    let target = TargetThreadId::new(u64::from(input.u16()));
                    if let Some(thread) = stop.threads.get_mut(thread) {
                        thread.futex = Some(futex);
                        thread.target = target;
                    }
                }
                5 => {
                    let set = stop.value(&mut input);
                    stop.task_sets.push(set);
                }
                _ => {
                    let object = stop.anchor(input.byte(), u64::from(input.u16()));
                    let field = pick(&fixture.fields, input.u16());
                    let value = stop.value(&mut input);
                    if let Some((offset, size)) = field {
                        let size = usize::try_from(size.clamp(1, 8)).unwrap_or(8);
                        let at = object.get().wrapping_add(offset);
                        stop.write(at, &value.to_le_bytes()[..size]);
                    }
                }
            }
        }

        stop
    }

    /// A word: an anchor and an offset from it, or the next eight bytes.
    fn value(&self, input: &mut Input<'_>) -> u64 {
        match input.byte() {
            0xff => input.u64(),
            0xfe => pick(&self.fixture.constants, input.u16()).unwrap_or(0),
            anchor => self.anchor(anchor, u64::from(input.u16())).get(),
        }
    }

    fn anchor(&self, anchor: u8, offset: u64) -> VirtualAddress {
        let anchors = &self.fixture.anchors;
        let base = if anchors.is_empty() {
            HEAP
        } else {
            anchors[usize::from(anchor) % anchors.len()]
        };
        VirtualAddress::new(base.wrapping_add(offset))
    }

    /// Fills `bytes` with what memory from `at` holds before the input
    /// writes it, a mapped range at a time, or says some of it is unmapped.
    fn original(&self, mut at: u64, mut bytes: &mut [u8]) -> bool {
        let heap = (HEAP, REGIONS * REGION);
        let threads = (THREADS, u64::from(MOST_THREADS) * THREAD_BLOCK);
        while !bytes.is_empty() {
            let within = |(start, size): (u64, u64)| at >= start && at - start < size;
            let (count, data) =
                if let Some((start, size)) = [heap, threads].into_iter().find(|&r| within(r)) {
                    (start + size - at, &[][..])
                } else if let Some((start, size, data)) = self
                    .fixture
                    .segments
                    .iter()
                    .find(|&&(start, size, _)| within((start, size)))
                {
                    let offset = usize::try_from(at - start).unwrap_or(usize::MAX);
                    (start + size - at, data.get(offset..).unwrap_or_default())
                } else {
                    return false;
                };

            let count = usize::try_from(count)
                .unwrap_or(usize::MAX)
                .min(bytes.len());
            let (chunk, rest) = bytes.split_at_mut(count);
            let copied = data.len().min(count);
            chunk[..copied].copy_from_slice(&data[..copied]);
            chunk[copied..].fill(0);
            bytes = rest;
            at += count as u64;
        }

        true
    }

    /// Writes `bytes` a page at a time, leaving out what is unmapped.
    fn write(&mut self, address: u64, mut bytes: &[u8]) {
        let mut at = address;
        while !bytes.is_empty() {
            let page = at - at % PAGE;
            let within = usize::try_from(at - page).unwrap_or(0);
            let count = bytes
                .len()
                .min(usize::try_from(PAGE).unwrap_or(4096) - within);
            let (chunk, rest) = bytes.split_at(count);

            if !self.pages.contains_key(&page)
                && self.pages.len() < MOST_PAGES
                && self.original(page, &mut [0])
            {
                // A page that runs past the end of what is mapped holds
                // zero there.
                let mut held = vec![0; usize::try_from(PAGE).unwrap_or(4096)].into_boxed_slice();
                if !self.original(page, &mut held) {
                    held.fill(0);
                    for (at, byte) in (page..).zip(held.iter_mut()) {
                        let _ = self.original(at, std::slice::from_mut(byte));
                    }
                }
                self.pages.insert(page, held);
            }

            if let Some(held) = self.pages.get_mut(&page) {
                held[within..within + count].copy_from_slice(chunk);
            }

            let Some(next) = at.checked_add(count as u64) else {
                break;
            };
            at = next;
            bytes = rest;
        }
    }

    fn thread(&self, thread: ThreadId) -> Option<&Thread> {
        self.threads.iter().find(|held| held.id == thread)
    }
}

impl RuntimeStop for Stop<'_> {
    fn read(&self, address: VirtualAddress, bytes: &mut [u8]) -> bool {
        let start = address.get();
        if start.checked_add(bytes.len() as u64).is_none() {
            return false;
        }

        let mut at = start;
        let mut rest = &mut bytes[..];
        while !rest.is_empty() {
            let page = at - at % PAGE;
            let within = usize::try_from(at - page).unwrap_or(0);
            let count = rest
                .len()
                .min(usize::try_from(PAGE).unwrap_or(4096) - within);
            let (chunk, after) = rest.split_at_mut(count);
            if let Some(held) = self.pages.get(&page) {
                chunk.copy_from_slice(&held[within..within + count]);
            } else if !self.original(at, chunk) {
                return false;
            }

            rest = after;
            at += count as u64;
        }

        true
    }

    fn thread_pointer(&self, thread: ThreadId) -> Option<u64> {
        self.thread(thread).map(|thread| thread.pointer)
    }

    fn module_thread_local(
        &self,
        thread: ThreadId,
        offset: u64,
    ) -> Result<VirtualAddress, Arc<str>> {
        let thread = self.thread(thread).ok_or("no such thread")?;
        Ok(VirtualAddress::new(
            (thread.pointer + DYNAMIC).wrapping_add(offset),
        ))
    }

    fn instruction(&self, thread: ThreadId) -> Option<VirtualAddress> {
        self.thread(thread)?
            .registers
            .get(RETURN_ADDRESS)
            .map(VirtualAddress::new)
    }

    fn load_bias(&self) -> crate::LoadBias {
        crate::LoadBias::default()
    }

    fn threads(&self) -> Vec<ThreadId> {
        self.threads.iter().map(|thread| thread.id).collect()
    }

    fn main_thread(&self) -> Option<ThreadId> {
        self.threads.first().map(|thread| thread.id)
    }

    fn target_thread_id(&self, thread: ThreadId) -> Option<TargetThreadId> {
        self.thread(thread)?.target
    }

    fn task_sets(&self) -> Vec<u64> {
        self.task_sets.clone()
    }

    fn blocked_futex(&self, thread: ThreadId) -> Option<u64> {
        self.thread(thread)?.futex
    }

    fn memory(&self) -> Option<&StopMemory> {
        self.memory.as_ref()
    }

    fn runs(&self) -> bool {
        self.runs
    }
}

/// The `index`th of `items`, counting round.
fn pick<T: Copy>(items: &[T], index: u16) -> Option<T> {
    (!items.is_empty()).then(|| items[usize::from(index) % items.len()])
}

/// What the input asks beside the stop: its first [`Query::BYTES`] bytes.
struct Query([u8; Self::BYTES]);

impl Query {
    const BYTES: usize = 8;

    fn new(data: &[u8]) -> Self {
        Self(std::array::from_fn(|index| {
            data.get(index).copied().unwrap_or(0)
        }))
    }

    const fn byte(&self, index: usize) -> u8 {
        self.0[index % Self::BYTES]
    }

    const fn bit(&self, bit: usize) -> bool {
        self.byte(1 + bit / 8) & (1 << (bit % 8)) != 0
    }
}

/// Asks `model` everything a session asks a runtime at a stop.
fn drive(fixture: &Fixture, model: &dyn RuntimeModel, stop: &Stop<'_>, query: &Query) {
    let _ = (model.signals(), model.task_noun(), model.driver_role());
    let _ = model.process_code(stop);
    let _ = model.inspector_agent(stop);
    let _ = model.inspector_endpoint(stop);

    tasks(fixture, model, stop, query);
    threads(model, stop, query);
    interpreted(model, stop, query);

    let labels = model.thread_labels(stop);
    debug_assert!(labels.value.len() <= stop.threads.len());
    if let Some(walked) = model.walked_threads(stop) {
        for thread in walked.value.iter().take(MOST_FRAMES) {
            let frames: Vec<SampledFrame> = (0..4_u64)
                .map(|depth| SampledFrame {
                    code: VirtualAddress::new(thread.state.get().wrapping_add(depth * 8)),
                    proof: SampledCode {
                        qualname: stop.anchor(query.byte(2), depth * 16).get(),
                        linetable: stop.anchor(query.byte(3), depth * 16).get(),
                        first_line: i32::from(query.byte(4)),
                    },
                    offset: Some(i64::from(query.byte(5))),
                    owner: u32::from(query.byte(6)),
                    function: Some(stop.anchor(query.byte(7), depth * 32).get()),
                })
                .collect();
            let _ = model.sampled_frames(stop, &frames);
        }
    }

    for anchor in 0..4 {
        let at = |index: u8| {
            stop.anchor(query.byte(usize::from(anchor + index)), 0)
                .get()
        };
        let frame = SampledCodeFrame {
            frame_pointer: at(0),
            instruction: VirtualAddress::new(at(1)),
            after_call: query.bit(usize::from(anchor)),
            generated: query.bit(usize::from(anchor) + 1),
            function: at(2),
            shared: at(3),
            script: at(4),
            script_id: u64::from(query.byte(5)),
            literal_id: i32::from(query.byte(6)),
            bytecode: at(7),
            bytecode_offset: u64::from(query.byte(4)),
        };
        let _ = model.sampled_code_frame(stop, &frame);
    }

    for name in ["py:main", "py:Shape.area", "js:handler", "js:main"] {
        let spec = crate::protocol::BreakpointSpec::Function(name.to_owned());
        let _ = model.resolve_breakpoint(stop, &spec);
    }

    let arming = Arming {
        breakpoints: &[],
        step: None,
        assignment: None,
        exceptions: crate::protocol::ExceptionStops::default(),
    };
    let _ = model.arm(stop, &arming);

    for representation in ["runtime.eface", "runtime.iface"] {
        let at = stop.anchor(query.byte(3), u64::from(query.byte(4)) * 8);
        let _ = model.dynamic_value(stop, representation, StoredValue::Memory(at));
        let bytes = stop.anchor(query.byte(5), 0).get().to_le_bytes();
        let mut both = [0; 16];
        both[..8].copy_from_slice(&bytes);
        both[8..].copy_from_slice(&bytes);
        let _ = model.dynamic_value(stop, representation, StoredValue::Bytes(&both));
    }

    for &ty in fixture.futures.iter().take(8) {
        let future = stop.anchor(query.byte(6), 0);
        let chain = futures::walk(fixture.image.as_ref(), stop, future, ty);
        let _ = model.held_task_sets(
            stop,
            &DrivenFuture {
                object: future,
                ty,
                chain: chain.frames,
                running: if query.byte(7) & 1 == 0 {
                    Vec::new()
                } else {
                    vec![ty]
                },
            },
        );
        let _ = model.may_run_task_set(ty);
    }

    let caller = stop.anchor(query.byte(2), 0);
    let delegate = stop.anchor(query.byte(3), 0);
    let _ = model.inspector_target(stop, caller, delegate);
    let _ = model.inspector_startup_pending(stop, delegate);
    let _ = model.inspector_message_matches(stop, caller, b"{\"id\":1}");
    let _ = model.inspector_received_matches(stop, delegate, b"{\"id\":1}");
}

/// Lists the runtime's tasks page by page, and asks after each.
fn tasks(fixture: &Fixture, model: &dyn RuntimeModel, stop: &Stop<'_>, query: &Query) {
    let limit = 1 + usize::from(query.byte(2)) % MOST_TASKS;
    let program_only = query.bit(0);
    let mut start = 0;
    let mut tasks = Vec::new();

    // A page's position is the runtime's own, in no order, but a session
    // that lists every task reads pages until there is no next: one that
    // comes back to a position never ends.
    let mut seen = BTreeSet::from([start]);
    for _ in 0..MOST_PAGES_LISTED {
        let page = model.tasks(stop, start, limit, program_only);
        debug_assert!(page.value.tasks.len() <= limit);
        tasks.extend(page.value.tasks);
        let Some(next) = page.value.next else {
            break;
        };

        assert!(seen.insert(next), "the pages of tasks come back to {next}");
        start = next;
    }

    for (index, task) in tasks.iter().take(MOST_TASKS).enumerate() {
        // Without its locator a task is found by searching every task,
        // which costs the same for each one, so only the first few are.
        let both = [Some(task.locator), None];
        let locators = if index < MOST_SEARCHED {
            &both[..]
        } else {
            &both[..1]
        };
        for &locator in locators {
            let task = TaskRef {
                number: task.number,
                locator,
            };
            if let Ok(Some(context)) = model.task_context(stop, task) {
                context_of(fixture, stop, &context);
            }

            let _ = model.task_stack(stop, task);
            if let Ok(Some(entries)) = model.task_entries(stop, task) {
                let _ = model.task_end(stop, &entries);
                for &entry in entries.runs.iter().chain(&entries.frees).take(8) {
                    for thread in &stop.threads {
                        let _ = model.takes_up(stop, &entries, entry, &thread.registers);
                    }
                }
            }
        }
    }
}

/// Follows where a task's frames begin as a backtrace would.
fn context_of(fixture: &Fixture, stop: &Stop<'_>, context: &TaskContext) {
    match context {
        TaskContext::Suspended { future, ty } => {
            let chain = futures::walk(fixture.image.as_ref(), stop, *future, *ty);
            debug_assert!(chain.frames.len() <= futures::MAX_DEPTH);
            let chosen = futures::walk_choosing(
                fixture.image.as_ref(),
                stop,
                *future,
                *ty,
                &mut |n, _, _| {
                    if n % 2 == 0 {
                        futures::StateChoice::Chosen(n as u64)
                    } else {
                        futures::StateChoice::Unknown
                    }
                },
            );
            debug_assert!(chosen.frames.len() <= futures::MAX_DEPTH);
        }
        TaskContext::OnThread(_) | TaskContext::Saved { .. } | TaskContext::Interpreted { .. } => {}
    }
}

/// Asks after each stopped thread, and what its registers say.
fn threads(model: &dyn RuntimeModel, stop: &Stop<'_>, query: &Query) {
    for thread in &stop.threads {
        let id = thread.id;
        let _ = model.thread_activity(stop, id);
        let _ = model.current_task(stop, id);
        let _ = model.thread_stacks(stop, id);
        let _ = model.thread_holding(stop, id);
        let _ = model.sole_thread(stop, id);

        let registers = &thread.registers;
        for after_call in [false, true] {
            let _ = model.cross(stop, id, registers, after_call);
        }

        for hook in model.hooks() {
            let _ = model.exception(stop, hook.address, registers);
        }

        for &hook in model.internal_hooks() {
            let _ = model.hook_event(stop, hook, registers);
        }

        let _ = model.moving_task(stop, registers);
        for starter in model.task_starters() {
            let _ = model.started_task(stop, starter.entry, registers);
        }

        for entry in code_places(model) {
            let _ = model.call_out(entry, registers);
        }

        let _ = model.native_callee(stop, registers);

        let Some(instruction) = registers.get(RETURN_ADDRESS) else {
            continue;
        };
        let instruction = VirtualAddress::new(instruction);
        let image_instruction = ImageAddress::new(instruction.get());
        let owned = model.owns_code(image_instruction);
        let frame = CodeFrame {
            thread: id,
            instruction,
            after_call: query.bit(8),
            generated: !owned && query.bit(9),
            image_instruction: owned.then_some(image_instruction),
            entered: query.bit(10),
            registers,
        };
        let _ = model.code_frame(stop, &frame);

        let stack_pointer = registers.get(RSP).unwrap_or(0);
        let code = |address: VirtualAddress| {
            let image = ImageAddress::new(address.get());
            if model.owns_code(image) {
                CodeAt::Image(image)
            } else if query.bit(11) {
                CodeAt::Generated
            } else {
                CodeAt::Other
            }
        };
        let live = LiveThread {
            thread: id,
            stack_pointer,
            stack: stack_pointer..stack_pointer.saturating_add(REGION),
            code: &code,
        };
        let _ = model.live_frames(stop, &live);
    }
}

/// Lists the interpreter's frames, and asks after their variables and
/// the objects they hold.
fn interpreted(model: &dyn RuntimeModel, stop: &Stop<'_>, query: &Query) {
    let Some(stacks) = model.interpreted(stop) else {
        return;
    };
    stacks.value.check_invariants();

    let mut budget = InspectionBudget::new(InspectionLimits::default());
    for (index, frame) in stacks.value.frames.iter().take(MOST_FRAMES).enumerate() {
        let reference = InterpretedFrameRef {
            address: frame.frame,
            live_registers: query.bit(12 + index % 4),
        };
        if let Ok(locals) = model.interpreted_locals(stop, reference, &mut budget) {
            for local in locals.iter().take(MOST_LOCALS) {
                slot(model, stop, &local.slot, 2, &mut budget);
            }
        }

        for name in NAMES {
            if let Ok(Some(InterpretedName::Object(object))) =
                model.interpreted_name(stop, reference, name, &mut budget)
            {
                object_of(model, stop, object, 1, &mut budget);
            }
        }

        for slot in 0..4 {
            if let Ok(object) = model.interpreted_variable(stop, reference, slot, &mut budget) {
                object_of(model, stop, object, 1, &mut budget);
            }

            let _ = model.assigned_binding(stop, reference, slot, &mut budget);
            if let Ok(watch) = model.interpreted_watch(stop, reference, slot, &mut budget) {
                for thread in &stop.threads {
                    let _ = model.interpreted_frame_runs(
                        stop,
                        thread.id,
                        frame.frame,
                        watch.code,
                        watch.address,
                    );
                }

                if let Ok(place) = model.interpreted_place(stop, frame.frame, watch.address) {
                    let _ = model.interpreted_placed(stop, frame.frame, place);
                }

                let mut word = [0; 8];
                if stop.read(watch.address, &mut word) {
                    let _ = model.interpreted_watched(u64::from_le_bytes(word));
                }
            }
        }
    }

    for anchor in 0..2 {
        let address = stop.anchor(query.byte(anchor), u64::from(query.byte(anchor + 4)) * 8);
        if let Ok(object) = model.interpreted_handle(stop, address, &mut budget) {
            object_of(model, stop, object, 1, &mut budget);
        }
    }
}

/// Asks after a variable's or child's value, and its children to `depth`.
fn slot(
    model: &dyn RuntimeModel,
    stop: &Stop<'_>,
    slot: &InterpretedSlot,
    depth: u32,
    budget: &mut InspectionBudget,
) {
    let InterpretedSlot::Value(value) = slot else {
        return;
    };

    let _ = model.interpreted_address(value.object);
    let _ = model.interpreted_lineage(stop, value.class, budget);
    let _ = model.interpreted_operand(stop, value.object, budget);
    let _ = model.interpreted_length(stop, value.object, budget);
    for key in [
        InterpretedKey::Int(0),
        InterpretedKey::Text(b"a".to_vec()),
        InterpretedKey::Tuple(vec![InterpretedKey::Nothing, InterpretedKey::Bool(true)]),
    ] {
        let _ = model.interpreted_step(stop, value.object, InterpretedStep::Item(&key), budget);
    }
    let _ = model.interpreted_step(stop, value.object, InterpretedStep::Attribute("x"), budget);

    if depth == 0 {
        return;
    }

    let children = value.children().min(MOST_CHILDREN);
    let Ok(children) =
        model.interpreted_children(stop, (value.object, &value.part), 0..children, budget)
    else {
        return;
    };
    for child in children {
        match child {
            InterpretedChild::Element(value) | InterpretedChild::Attribute { value, .. } => {
                self::slot(model, stop, &value, depth - 1, budget);
            }
            InterpretedChild::Entry { key, value } => {
                self::slot(model, stop, &key, depth - 1, budget);
                self::slot(model, stop, &value, depth - 1, budget);
            }
        }
    }
}

fn object_of(
    model: &dyn RuntimeModel,
    stop: &Stop<'_>,
    object: InterpretedHandle,
    depth: u32,
    budget: &mut InspectionBudget,
) {
    let value = model.interpreted_value(stop, object, budget);
    slot(model, stop, &value, depth, budget);
}
