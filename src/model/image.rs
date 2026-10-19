//! A module image's static metadata and the indexes that answer lookups
//! in it.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::image::functions::{CodeInstance, Function, FunctionView};
use crate::image::lines::LineView;
use crate::image::symbols::{Symbol, SymbolView};
use crate::type_identity::NameIndex as _;
use crate::{Error, Result};

mod locations;

pub use locations::PackageInfo;

use super::{
    AddressRange, BreakpointEntry, CodeInstanceId, CodeInstanceInfo, CodeInstanceKind, CodeRole,
    FunctionId, FunctionInfo, GlobalVariableId, GlobalVariableInfo, GotSlot, ImageAddress,
    ImageAddressDescription, ImageLocation, InlineChain, InlineFrameLookup, LineEntry, LineNumber,
    ModuleImageId, SectionId, SectionInfo, SectionLocation, SourceFile, SourceFileId,
    SourceLanguage, SourceLocation, StatementRow, SymbolExtentProvenance, SymbolId, SymbolInfo,
    SymbolKind, SymbolLocation, SymbolTableSources, TargetDescription, TypeId, TypeInfo, TypeNode,
    TypeReference,
};

#[derive(Default)]
pub struct ModuleMetadata {
    pub functions: Vec<FunctionInfo>,
    pub code_instances: Vec<CodeInstanceInfo>,
    pub symbols: Vec<SymbolInfo>,
    pub symbol_sources: SymbolTableSources,

    /// The GOT slots the loader fills with functions' addresses.
    pub got_slots: Vec<GotSlot>,
    pub types: Arc<[TypeNode]>,

    /// Where variables are: expressions and lists of them, with the units
    /// they were read from.
    pub locations: crate::image::locations::LocationsBuilder,

    /// The data objects and the functions whose frames show them.
    pub variables: crate::image::variables::Variables,

    /// The calls the functions make.
    pub calls: crate::image::calls::Calls,

    /// What reading values of some types takes beyond their layout.
    pub type_facts: crate::image::type_facts::TypeFacts,

    /// Source files by resolved path.
    pub files: crate::image::lines::Files,

    /// Every line-program row and the code each line describes.
    pub lines: crate::image::lines::LineTables,

    /// The call-frame sections and Go's table, as unwinding reads them.
    pub unwind: Option<crate::image::unwind::Unwind>,
    pub sections: Vec<SectionInfo>,

    /// Where each out-of-line code instance that runs a coroutine goes for
    /// each state, and which variables of async bodies hold their values
    /// on resuming.
    pub resumes: crate::image::resumes::Resumes,

    /// Whether each thread gets its own copy of a block of the module's
    /// storage.
    pub thread_local_storage: bool,

    /// The Python runtime the module carries.
    pub python: Option<crate::image::python::PythonRuntime>,

    /// Integer constants the debug information declares by name, such as a
    /// Go package's `const`s, Rust trait objects' vtables, and the
    /// compilers and versions that produced the debug information, as each
    /// unit names its producer.
    pub declarations: crate::image::declarations::Declarations,

    /// The packages whose units the image has.
    pub packages: Vec<PackageInfo>,

    /// The separate debug file found for the image, whether it was used or
    /// could not be.
    pub debug_file: Option<crate::DebugFile>,

    /// What became of the image's DWARF.
    pub debug_information: crate::DebugInformation,

    /// Why the DWARF's locals and parameters were left out, when they
    /// were.
    pub locals_left_out: Option<Arc<str>>,

    /// The bytes of the image's `.debug_microscope_views` section, which holds
    /// views for its own types; empty when it has none.
    pub embedded_views: Vec<u8>,

    /// Where each thread's copy of each of the image's thread-local
    /// variables is, by name, or why that is unknown.
    pub thread_locals: BTreeMap<Arc<str>, std::result::Result<ThreadLocal, Arc<str>>>,
}

/// Where a thread's copy of a thread-local variable is, relative to the
/// thread's thread pointer, as the image's own code finds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadLocal {
    /// This far from it, fixed when the program was linked, as an
    /// executable's own thread-locals are.
    Offset(i64),

    /// As far from it as the word at this address says, which the loader
    /// writes as it loads the image, as a library's code reads its
    /// thread-locals.
    Slot(ImageAddress),
}

/// An image's types by name and base, as name lookups search them.
struct TypeNames<'a>(&'a crate::image::types::TypeTable);

impl crate::type_identity::NameIndex for TypeNames<'_> {
    fn image(&self) -> Option<ModuleImageId> {
        Some(self.0.image())
    }

    fn by_name(&self, name: &str) -> Vec<TypeId> {
        self.0.view().named(name).collect()
    }

    fn by_base(&self, base: &str) -> Vec<TypeId> {
        self.0.view().with_base(base).collect()
    }
}

#[derive(Debug)]
struct RangeIndexEntry<T> {
    start: u64,
    end: u64,
    prefix_max_end: u64,
    value: T,
}

#[derive(Debug)]
struct RangeIndex<T> {
    entries: Arc<[RangeIndexEntry<T>]>,
}

impl<T: Copy + Ord> RangeIndex<T> {
    fn new(entries: impl IntoIterator<Item = (AddressRange<ImageAddress>, T)>) -> Self {
        let mut entries = entries
            .into_iter()
            .map(|(range, value)| RangeIndexEntry {
                start: range.start.get(),
                end: range.end.get(),
                prefix_max_end: 0,
                value,
            })
            .collect::<Vec<_>>();
        entries.sort_unstable_by_key(|entry| (entry.start, entry.end, entry.value));

        let mut prefix_max_end = 0;

        for entry in &mut entries {
            prefix_max_end = prefix_max_end.max(entry.end);
            entry.prefix_max_end = prefix_max_end;
        }

        Self {
            entries: entries.into(),
        }
    }

    fn containing(&self, address: ImageAddress) -> impl Iterator<Item = T> + '_ {
        let address = address.get();
        let mut index = self.entries.partition_point(|entry| entry.start <= address);

        std::iter::from_fn(move || {
            while index > 0 {
                index -= 1;
                let entry = &self.entries[index];
                if entry.prefix_max_end <= address {
                    return None;
                }

                if address < entry.end {
                    return Some(entry.value);
                }
            }

            None
        })
    }
}

/// Groups values by key, each group sorted and without duplicates.
fn grouped_index<K: Ord, V: Ord>(
    entries: impl IntoIterator<Item = (K, V)>,
) -> BTreeMap<K, Arc<[V]>> {
    // Sorting once and taking each key's run is far cheaper than inserting
    // every entry into a tree of trees.
    let mut entries = entries.into_iter().collect::<Vec<_>>();
    entries.sort_unstable();
    entries.dedup_by(|later, earlier| (*later).cmp(earlier).is_eq());
    let mut grouped = Vec::new();
    let mut entries = entries.into_iter().peekable();

    while let Some((key, value)) = entries.next() {
        let mut values = vec![value];
        while let Some((_, value)) = entries.next_if(|(next, _)| next.cmp(&key).is_eq()) {
            values.push(value);
        }

        grouped.push((key, Arc::from(values)));
    }

    grouped.into_iter().collect()
}

/// Every selector naming a global: its name, qualified name, and linkage
/// name, and its qualified name after its declaring file's path or name.
fn global_selectors(image: &ModuleImage) -> Vec<(Arc<str>, GlobalVariableId)> {
    let mut selectors = Vec::new();
    for global in image.globals() {
        selectors.push((Arc::clone(&global.name), global.id));
        selectors.push((Arc::clone(&global.qualified_name), global.id));
        if let Some(linkage_name) = &global.linkage_name {
            selectors.push((Arc::clone(linkage_name), global.id));
        }

        if let Some(declaration) = &global.declaration
            && let Some(source) = image.source_file(declaration.file)
        {
            let path = source.path.to_string_lossy();
            selectors.push((
                format!("{path}::{}", global.qualified_name).into(),
                global.id,
            ));
            if let Some(file_name) = source.path.file_name() {
                let file_name = file_name.to_string_lossy();
                selectors.push((
                    format!("{file_name}::{}", global.qualified_name).into(),
                    global.id,
                ));
            }
        }
    }

    selectors
}

/// Where each function symbol with an extent begins, in order.
fn function_starts(symbols: &[SymbolInfo]) -> Vec<ImageAddress> {
    let mut starts = symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Function && symbol.extent.is_some())
        .map(|symbol| symbol.address)
        .collect::<Vec<_>>();
    starts.sort_unstable();
    starts.dedup();
    starts
}

/// The source files an image's tables name.
fn source_files(tables: &crate::image::Image) -> Arc<[SourceFile]> {
    let paths = crate::image::Paths(tables.bytes(crate::image::TableKind::Paths));
    tables
        .table::<crate::image::lines::FileRecord>()
        .iter()
        .zip(0u32..)
        .map(|(file, index)| SourceFile {
            id: SourceFileId::new(index),
            path: Arc::new(
                paths
                    .get(crate::image::PathId(file.path.get()))
                    .to_path_buf(),
            ),
        })
        .collect()
}

/// Why metadata could not be sealed as an image.
pub type SealError = Box<dyn std::error::Error + Send + Sync>;

/// Seals a module's metadata as an image.
pub fn seal(
    target: TargetDescription,
    address_range: AddressRange<ImageAddress>,
    mut metadata: ModuleMetadata,
) -> std::result::Result<crate::image::Image, SealError> {
    validate_dense_ids(&metadata);
    metadata.lines.clip_at(&function_starts(&metadata.symbols));
    crate::count!("types", metadata.types.len());
    let phase = crate::span!("image.seal");
    let tables = seal_image(target, address_range, metadata)?;
    drop(phase);
    crate::count!("image_bytes", tables.as_bytes().len());

    Ok(tables)
}

/// Frees what was sealed on a thread of its own. The loader's records hold
/// millions of allocations, which took a fifth of a second to free in
/// turn, and each is freed as soon as its tables are built, so that the
/// records and the image are not all held at once; the loader's workers
/// may all be busy.
fn free<T: Send + 'static>(sealed: T) {
    std::thread::spawn(move || drop(sealed));
}

fn seal_image(
    target: TargetDescription,
    address_range: AddressRange<ImageAddress>,
    mut metadata: ModuleMetadata,
) -> std::result::Result<crate::image::Image, SealError> {
    let variables = std::mem::take(&mut metadata.variables);
    let types = std::mem::take(&mut metadata.types);
    let functions = std::mem::take(&mut metadata.functions);
    let code_instances = std::mem::take(&mut metadata.code_instances);
    let metadata = &metadata;

    // Strings are pooled in the order the tables are encoded, which names
    // each, so the tables that pool strings are encoded in turn on this
    // thread while those that pool none are built beside them. Tables are
    // laid out by kind, whatever order they are added in.
    let mut builder = crate::image::Builder::new(target);
    let mut strings = crate::image::StringsBuilder::default();
    let ((classes, code), before) = rayon::join(
        || {
            rayon::join(
                || {
                    let _phase = crate::span!("image.type_classes");
                    type_classes(&types)
                },
                || encode_without_strings(target, metadata, &code_instances),
            )
        },
        || {
            encode_strings_before_types(
                &mut builder,
                &mut strings,
                address_range,
                metadata,
                variables,
            )
        },
    );

    before?;
    builder.absorb(code?);
    let phase = crate::span!("image.encode_types");
    crate::image::types::add_to(
        &mut builder,
        &mut strings,
        &crate::image::types::Types {
            nodes: &types,
            classes: &classes?,
        },
    )?;
    drop(phase);
    free(types);
    crate::image::packages::add_to(
        &mut builder,
        &mut strings,
        metadata
            .packages
            .iter()
            .map(|package| (&*package.path, &*package.name)),
        &locations::packaged_names(&functions, &metadata.packages),
    )?;
    crate::image::functions::add_functions_to(
        &mut builder,
        &mut strings,
        &functions,
        &code_instances,
    )?;
    free((functions, code_instances));

    if let Some(unwind) = &metadata.unwind {
        crate::image::unwind::add_to(&mut builder, &mut strings, unwind)?;
    }

    crate::image::python::add_to(&mut builder, &mut strings, metadata.python.as_ref())?;

    builder
        .bytes(
            crate::image::TableKind::EmbeddedViews,
            metadata.embedded_views.clone(),
        )
        .bytes(crate::image::TableKind::Strings, strings.into_bytes());

    builder
        .seal(crate::image::Limits::default())
        .map_err(SealError::from)
}

/// The tables that pool no string: the lines, the files and their paths,
/// the locations, and the code instances.
fn encode_without_strings<'a>(
    target: TargetDescription,
    metadata: &'a ModuleMetadata,
    code_instances: &[CodeInstanceInfo],
) -> std::result::Result<crate::image::Builder<'a>, SealError> {
    let _phase = crate::span!("image.encode_without_strings");
    let mut builder = crate::image::Builder::new(target);
    let mut paths = crate::image::PathsBuilder::default();
    metadata.lines.add_to(&mut builder);
    metadata
        .files
        .add_to(&mut builder, &mut paths)
        .ok_or("a source path holds a NUL, or the paths do not fit")?;
    metadata.locations.add_to(&mut builder);
    crate::image::functions::add_code_to(
        &mut builder,
        &crate::image::functions::Code {
            instances: code_instances,
            prologue_ends: &metadata.lines.prologue_ends(),
            instruction_starts: &instruction_starts(metadata, code_instances),
        },
    )?;

    builder.bytes(crate::image::TableKind::Paths, paths.into_bytes());
    Ok(builder)
}

/// The tables that pool strings before the types do, in the order they
/// pool them.
fn encode_strings_before_types<'a>(
    builder: &mut crate::image::Builder<'a>,
    strings: &mut crate::image::StringsBuilder,
    address_range: AddressRange<ImageAddress>,
    metadata: &'a ModuleMetadata,
    variables: crate::image::variables::Variables,
) -> std::result::Result<(), SealError> {
    crate::image::symbols::add_to(
        builder,
        strings,
        &metadata.symbols,
        &metadata.sections,
        &metadata.got_slots,
    )?;
    crate::image::facts::add_to(
        builder,
        strings,
        &crate::image::facts::Facts {
            address_range,
            symbol_sources: &metadata.symbol_sources,
            thread_local_storage: metadata.thread_local_storage,
            thread_locals: &metadata.thread_locals,
            debug_file: metadata.debug_file.as_ref(),
            debug_information: &metadata.debug_information,
            locals_left_out: metadata.locals_left_out.as_deref(),
        },
    )?;
    let phase = crate::span!("image.encode_variables");
    crate::image::variables::add_to(builder, strings, &variables)?;
    drop(phase);
    free(variables);
    crate::image::calls::add_to(builder, strings, &metadata.calls)?;
    crate::image::type_facts::add_to(builder, strings, &metadata.type_facts)?;
    crate::image::resumes::add_to(builder, strings, &metadata.resumes)?;
    crate::image::declarations::add_to(builder, strings, &metadata.declarations)
        .map_err(SealError::from)
}

/// Each type's identity class, numbered in the order classes first come:
/// two types share one when their identity keys are equal, as one type
/// defined in several units is.
fn type_classes(types: &[TypeNode]) -> std::result::Result<Vec<u32>, SealError> {
    // Only the keys are read, so the names are not indexed.
    let keys = crate::type_identity::canonical_keys(types.len(), |index| match &types[index] {
        TypeNode::Resolved(info) => Some(info),
        TypeNode::Malformed { .. } => None,
    });

    // There are no more classes than types, so each numbers below them.
    u32::try_from(keys.len()).map_err(|_| crate::image::types::TooMany)?;
    let mut classes = foldhash::HashMap::default();
    let mut next = 0_u32;

    Ok(keys
        .iter()
        .map(|key| {
            *classes.entry(&**key).or_insert_with(|| {
                next += 1;
                next - 1
            })
        })
        .collect())
}

fn validate_dense_ids(metadata: &ModuleMetadata) {
    for (index, function) in metadata.functions.iter().enumerate() {
        assert_eq!(
            function.id.index(),
            index,
            "function IDs are dense and ordered"
        );
    }

    for (index, instance) in metadata.code_instances.iter().enumerate() {
        assert_eq!(
            instance.id.index(),
            index,
            "code instance IDs are dense and ordered"
        );
    }

    for (index, symbol) in metadata.symbols.iter().enumerate() {
        assert_eq!(symbol.id.index(), index, "symbol IDs are dense and ordered");
        if let Some(extent) = symbol.extent {
            assert!(
                matches!(
                    symbol.kind,
                    SymbolKind::Function | SymbolKind::IndirectFunction
                ) && extent.range.start == symbol.address
                    && extent.range.start < extent.range.end,
                "symbol extents are non-empty code ranges beginning at the symbol"
            );
        }

        if let Some(storage) = symbol.storage {
            assert!(
                symbol.kind == SymbolKind::Data
                    && symbol.extent.is_none()
                    && storage.start == symbol.address
                    && storage.start <= storage.end,
                "symbol storage is a data range beginning at the symbol"
            );
        }
    }

    for (index, section) in metadata.sections.iter().enumerate() {
        assert_eq!(
            section.id.index(),
            index,
            "section IDs are dense and ordered"
        );
        assert!(
            section.range.start < section.range.end,
            "sections are non-empty"
        );
    }

    for (index, node) in metadata.types.iter().enumerate() {
        assert_eq!(
            node.reference().id.get() as usize,
            index,
            "type IDs are dense and ordered"
        );
    }
}

/// What binds an image's bytes to one module of a session: the file it
/// describes, the separate debug file found for it, and its identifier.
/// None of it is in the bytes, so one image serves every binding.
#[derive(Debug, Clone)]
pub struct Binding {
    pub path: Arc<PathBuf>,
    pub debug_path: Option<Arc<PathBuf>>,
    pub id: ModuleImageId,
}

/// Immutable, normalized debug metadata for one ELF module image.
#[derive(Debug)]
pub struct ModuleImage {
    id: ModuleImageId,
    path: Arc<PathBuf>,
    target: TargetDescription,
    address_range: AddressRange<ImageAddress>,
    got_slots: Arc<[GotSlot]>,
    sections: Arc<[SectionInfo]>,

    /// The type graph, decoded as it is asked for.
    types: Arc<crate::image::types::TypeTable>,
    source_files: Arc<[SourceFile]>,

    /// Lines, files, symbols, sections, functions, and code, as tables.
    tables: Arc<crate::image::Image>,

    /// Symbols by the last part of each name they answer to, built on the
    /// first search for one, since it demangles every symbol.
    symbols_by_last_part: std::sync::OnceLock<HashMap<Box<str>, Vec<SymbolId>>>,

    /// The functions that run each coroutine type, by the type's identity,
    /// built on the first search for one.
    coroutine_functions: std::sync::OnceLock<HashMap<u32, Vec<FunctionId>>>,

    /// Where each out-of-line instance of a function that begins a panic
    /// begins, found on the first search.
    panic_entries: std::sync::OnceLock<Arc<[ImageAddress]>>,

    /// Globals by every selector naming one, built on the first search by
    /// one.
    globals_by_selector: std::sync::OnceLock<BTreeMap<Arc<str>, Arc<[GlobalVariableId]>>>,

    /// The dispatches and leads of every decoded coroutine, which no
    /// breakpoint or step stops in.
    resume_code: RangeIndex<CodeInstanceId>,

    /// The views the image carries for its own types, in its
    /// `.debug_microscope_views` section, read on first use.
    views: std::sync::OnceLock<Arc<crate::view::ViewSet>>,

    /// The separate debug file found for the image.
    debug_file: Option<crate::DebugFile>,
}

impl ModuleImage {
    /// Seals `metadata` and binds it to `path` as module 0: for tests and
    /// fuzzing, which build metadata by hand.
    #[cfg(any(test, feature = "fuzzing"))]
    #[allow(
        clippy::expect_used,
        reason = "tests and the fuzz harness build images by hand"
    )]
    pub(crate) fn new(
        path: PathBuf,
        target: TargetDescription,
        address_range: AddressRange<ImageAddress>,
        metadata: ModuleMetadata,
    ) -> Self {
        let debug_path = metadata.debug_file.as_ref().map(|file| match file {
            crate::DebugFile::Used(path) | crate::DebugFile::Unusable { path, .. } => {
                Arc::clone(path)
            }
        });
        let tables = Arc::new(seal(target, address_range, metadata).expect("the metadata seals"));

        Self::bind(
            &Binding {
                path: Arc::new(path),
                debug_path,
                id: ModuleImageId::new(0),
            },
            tables,
        )
        .expect("an image binds to the files it was built from")
    }

    /// The module that `binding` names, described by `tables`: the image
    /// the loader sealed for it, or the same bytes read back from a cache.
    pub(crate) fn bind(
        binding: &Binding,
        tables: Arc<crate::image::Image>,
    ) -> std::result::Result<Self, crate::image::facts::Unbound> {
        let facts = crate::image::facts::FactsView::new(&tables);
        let debug_file = facts.debug_file(binding.debug_path.clone())?;

        Ok(Self {
            symbols_by_last_part: std::sync::OnceLock::new(),
            coroutine_functions: std::sync::OnceLock::new(),
            panic_entries: std::sync::OnceLock::new(),
            globals_by_selector: std::sync::OnceLock::new(),
            id: binding.id,
            path: Arc::clone(&binding.path),
            target: tables.target(),
            address_range: facts.address_range(),
            got_slots: crate::image::symbols::got_slots(&tables).into(),
            sections: crate::image::symbols::sections(&tables).into(),
            types: Arc::new(crate::image::types::TypeTable::new(
                Arc::clone(&tables),
                binding.id,
            )),
            source_files: source_files(&tables),
            resume_code: RangeIndex::new(
                crate::image::resumes::ResumeView::new(&tables).resume_code(),
            ),
            views: std::sync::OnceLock::new(),
            debug_file,
            tables,
        })
    }

    /// The separate debug file the image's debug information and symbols
    /// came from, when its own file was stripped of them.
    #[must_use]
    pub const fn debug_file(&self) -> Option<&Arc<PathBuf>> {
        match &self.debug_file {
            Some(crate::DebugFile::Used(path)) => Some(path),
            _ => None,
        }
    }

    /// The separate debug file found for the image, whether it was used or
    /// could not be.
    #[must_use]
    pub const fn separate_debug_file(&self) -> Option<&crate::DebugFile> {
        self.debug_file.as_ref()
    }

    /// What became of the image's DWARF: whether it was read, and when not
    /// all of it was, why.
    #[must_use]
    pub fn debug_information(&self) -> crate::DebugInformation {
        self.facts().debug_information()
    }

    /// Why the image's DWARF describes no locals or parameters of its
    /// functions, when they were left out to load the rest.
    #[must_use]
    pub fn locals_left_out(&self) -> Option<Arc<str>> {
        self.facts().locals_left_out()
    }

    /// The views the image carries for its own types.
    #[must_use]
    pub(crate) fn views(&self) -> &Arc<crate::view::ViewSet> {
        self.views.get_or_init(|| {
            let bytes = self.tables.bytes(crate::image::TableKind::EmbeddedViews);
            if bytes.is_empty() {
                return crate::view::ViewSet::empty();
            }

            // Views are named after the module's file.
            let module = self
                .path
                .file_name()
                .map_or_else(|| "module".into(), |name| name.to_string_lossy());
            Arc::new(crate::view::embedded::view_set(&module, bytes))
        })
    }

    /// A type's identity class, which every type the same as it shares.
    #[must_use]
    pub(crate) fn type_class(&self, reference: TypeReference) -> Option<u32> {
        if reference.image != self.id {
            return None;
        }

        self.types.view().class(reference.id)
    }

    /// The image's type graph, which its variable provider shares.
    pub(crate) const fn type_table(&self) -> &Arc<crate::image::types::TypeTable> {
        &self.types
    }

    /// What kept parts of the views the image carries out.
    #[must_use]
    pub fn view_errors(&self) -> &[crate::ViewFileError] {
        self.views().errors()
    }

    /// Returns this image's session-scoped identifier.
    #[must_use]
    pub const fn id(&self) -> ModuleImageId {
        self.id
    }

    /// Returns the executable path used to load this image.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn path_arc(&self) -> Arc<PathBuf> {
        Arc::clone(&self.path)
    }

    /// Returns the properties of this image's target.
    #[must_use]
    pub const fn target(&self) -> TargetDescription {
        self.target
    }

    /// Returns whether an image address lies in this module's loadable range.
    #[must_use]
    pub fn contains_address(&self, address: ImageAddress) -> bool {
        self.address_range.contains(address)
    }

    /// Returns the image addresses this module's loadable segments span.
    #[must_use]
    pub const fn address_range(&self) -> AddressRange<ImageAddress> {
        self.address_range
    }

    /// Returns all functions described by this image.
    pub fn functions(&self) -> impl ExactSizeIterator<Item = Function<'_>> + DoubleEndedIterator {
        self.function_view().functions()
    }

    /// Looks up a source-level function by identifier.
    #[must_use]
    pub fn function(&self, id: FunctionId) -> Option<Function<'_>> {
        self.function_view().function(id)
    }

    /// Returns all concrete code instances described by this image.
    pub fn code_instances(
        &self,
    ) -> impl ExactSizeIterator<Item = CodeInstance<'_>> + DoubleEndedIterator {
        self.function_view().instances()
    }

    fn function_view(&self) -> FunctionView<'_> {
        FunctionView::new(&self.tables)
    }

    fn symbol_view(&self) -> SymbolView<'_> {
        SymbolView::new(&self.tables)
    }

    /// Returns all linker symbols described by this image, ordered by
    /// address and then name.
    pub fn symbols(&self) -> impl ExactSizeIterator<Item = Symbol<'_>> + DoubleEndedIterator {
        self.symbol_view().all()
    }

    /// Looks up a linker symbol by identifier.
    #[must_use]
    pub fn symbol(&self, id: SymbolId) -> Option<Symbol<'_>> {
        self.symbol_view().get(id)
    }

    /// Returns the image's allocated sections, ordered by address.
    #[must_use]
    pub fn sections(&self) -> &[SectionInfo] {
        &self.sections
    }

    /// Looks up an allocated section by identifier.
    #[must_use]
    pub fn section(&self, id: SectionId) -> Option<&SectionInfo> {
        self.sections.get(id.index())
    }

    /// Whether each thread gets its own copy of a block of the module's
    /// storage, such as an ELF `PT_TLS` segment describes.
    #[must_use]
    pub fn has_thread_local_storage(&self) -> bool {
        self.facts().thread_local_storage()
    }

    fn facts(&self) -> crate::image::facts::FactsView<'_> {
        crate::image::facts::FactsView::new(&self.tables)
    }

    /// Finds the allocated section containing an image address. Should
    /// malformed sections overlap, the innermost one wins.
    fn section_containing(&self, address: ImageAddress) -> Option<&SectionInfo> {
        crate::image::index::containing(
            self.tables.shared(crate::image::TableKind::SectionRanges),
            address,
        )
        .filter_map(|id| self.section(SectionId::new(id)))
        .min_by_key(|section| {
            (
                std::cmp::Reverse(section.range.start),
                section.range.end,
                section.id,
            )
        })
    }

    /// Returns which symbol tables this image provided.
    #[must_use]
    pub fn symbol_sources(&self) -> SymbolTableSources {
        self.facts().symbol_sources()
    }

    /// Finds the code symbol whose extent contains an image address.
    ///
    /// Declared extents take precedence over inferred ones. Among the
    /// remaining candidates the innermost extent wins, then a function over an
    /// indirect-function resolver, then global over weak over local binding,
    /// then an exported symbol, then the name with the fewest leading
    /// underscores, then the bytewise-smallest name. An address that no extent
    /// contains has no symbol; the nearest preceding symbol is never guessed.
    #[must_use]
    pub fn symbolize(&self, address: ImageAddress) -> Option<SymbolLocation> {
        let symbol = self.symbol_view().code_at(address)?;
        let extent = symbol.extent();
        debug_assert!(
            extent.is_some(),
            "validation checked indexed symbols have extents"
        );
        let extent = extent?;

        Some(SymbolLocation {
            symbol: symbol.id(),
            name: symbol.name().into(),
            kind: symbol.kind(),
            offset: address.get() - symbol.address().get(),
            provenance: extent.provenance,
        })
    }

    /// Finds the data symbol naming an image address: the one whose declared
    /// storage contains it, choosing among overlapping storage as
    /// [`Self::symbolize`] chooses among code extents, or otherwise an
    /// unsized data symbol at exactly that address. An address inside no
    /// declared storage is never attributed to the nearest preceding object.
    fn symbolize_data(&self, address: ImageAddress) -> Option<SymbolLocation> {
        let symbol = self.symbol_view().data_at(address)?;
        let storage = symbol.storage();
        debug_assert!(
            storage.is_some(),
            "validation checked indexed symbols have storage"
        );
        let storage = storage?;

        Some(SymbolLocation {
            symbol: symbol.id(),
            name: symbol.name().into(),
            kind: symbol.kind(),
            offset: address.get() - symbol.address().get(),
            provenance: if storage.start < storage.end {
                SymbolExtentProvenance::Declared
            } else {
                SymbolExtentProvenance::Inferred
            },
        })
    }

    /// Returns the addresses within `range` known to begin an instruction:
    /// the start of every range of an out-of-line function instance, of
    /// every code symbol with an extent, and of every executable section.
    /// Line table rows are excluded because some producers emit rows inside
    /// instructions, such as Go after a `LOCK` prefix.
    pub fn instruction_starts(
        &self,
        range: AddressRange<ImageAddress>,
    ) -> impl Iterator<Item = (ImageAddress, crate::BoundaryEvidence)> + '_ {
        self.function_view().instruction_starts(range)
    }

    /// Returns the source line containing an image address, when the line
    /// table describes it.
    #[must_use]
    pub fn source_location(&self, address: ImageAddress) -> Option<SourceLocation> {
        self.line_entry_containing(address)
            .map(|entry| entry.location)
    }

    /// Describes an image address by its section and by the code symbol, or
    /// otherwise the data symbol, containing it.
    #[must_use]
    pub fn describe(&self, address: ImageAddress) -> ImageAddressDescription {
        ImageAddressDescription {
            address,
            section: self
                .section_containing(address)
                .map(|section| SectionLocation {
                    section: section.id,
                    name: Arc::clone(&section.name),
                    offset: address.get() - section.range.start.get(),
                    executable: section.executable,
                }),
            symbol: self
                .symbolize(address)
                .or_else(|| self.symbolize_data(address)),
        }
    }

    /// Where each thread's copy of the named thread-local variable is, or
    /// why that is unknown; `None` when the image defines none by the name.
    #[must_use]
    pub fn thread_local(&self, name: &str) -> Option<std::result::Result<ThreadLocal, Arc<str>>> {
        self.facts().thread_local(name)
    }

    /// Where each thread's copy is of the one thread-local variable whose
    /// demangled name lies within `scope` and ends in `name`, as the
    /// storage std's `thread_local!` makes is named within the variable's
    /// own scope; `None` when the image defines none.
    #[must_use]
    pub fn thread_local_within(
        &self,
        scope: &str,
        name: &str,
    ) -> Option<std::result::Result<ThreadLocal, Arc<str>>> {
        let mut found = self.facts().thread_locals().filter(|(symbol, _)| {
            symbol.contains(name)
                && crate::demangle::demangle(symbol).is_some_and(|demangled| {
                    demangled
                        .strip_prefix(scope)
                        .is_some_and(|rest| rest.starts_with("::"))
                        && demangled
                            .strip_suffix(name)
                            .is_some_and(|rest| rest.ends_with("::"))
                })
        });
        let (_, place) = found.next()?;

        Some(if found.next().is_some() {
            Err(format!("several thread-local variables are named {name} within {scope}").into())
        } else {
            place
        })
    }

    /// Returns every global catalog entry in deterministic source order.
    pub fn globals(&self) -> impl Iterator<Item = GlobalVariableInfo> + '_ {
        let view = crate::image::variables::VariableView::new(&self.tables);
        (0..view.global_count()).filter_map(move |index| {
            let global = self.decode_global(view, index);
            debug_assert!(global.is_some(), "global {index} in range is in the table");
            global
        })
    }

    /// Looks up a global catalog entry by identifier.
    #[must_use]
    pub fn global(&self, id: GlobalVariableId) -> Option<GlobalVariableInfo> {
        self.decode_global(
            crate::image::variables::VariableView::new(&self.tables),
            id.index(),
        )
    }

    fn decode_global(
        &self,
        view: crate::image::variables::VariableView<'_>,
        index: usize,
    ) -> Option<GlobalVariableInfo> {
        use crate::image::variables::TypeResolution;

        let entry = view.global_entry(index)?;
        let malformed = |description| {
            crate::GlobalVariableType::Malformed(crate::VariableMalformedReason {
                kind: crate::VariableMalformedKind::InvalidTypeGraph,
                description,
            })
        };

        Some(GlobalVariableInfo {
            id: GlobalVariableId::new(u32::try_from(index).ok()?),
            name: entry.object.name().into(),
            qualified_name: entry.qualified_name.into(),
            linkage_name: entry.linkage_name.map(Arc::from),
            declaration: entry.object.declaration(),
            type_info: match entry.object.type_info() {
                TypeResolution::Resolved(ty) => match self.types.node(ty) {
                    Some(TypeNode::Resolved(info)) => {
                        crate::GlobalVariableType::Resolved(info.clone())
                    }
                    Some(TypeNode::Malformed { description, .. }) => {
                        malformed(Arc::clone(description))
                    }
                    None => malformed("type graph did not finish building".into()),
                },
                TypeResolution::Malformed(description) => malformed(description),
            },
            visibility: if entry.external {
                crate::GlobalVariableVisibility::External
            } else {
                crate::GlobalVariableVisibility::CompilationUnit
            },
        })
    }

    /// Returns the reachable, normalized type graph in stable identifier
    /// order, decoding every type.
    pub fn types(&self) -> impl ExactSizeIterator<Item = &TypeNode> + '_ {
        self.types.nodes()
    }

    /// How many types the image has.
    #[must_use]
    pub fn type_count(&self) -> usize {
        self.types.len()
    }

    /// Resolves a reference owned by this image to its finalized graph node.
    #[must_use]
    pub fn type_node(&self, reference: TypeReference) -> Option<&TypeNode> {
        if reference.image != self.id {
            return None;
        }

        self.types.node(reference.id)
    }

    /// Resolves a reference to normalized metadata when the node is not malformed.
    #[must_use]
    pub fn type_info(&self, reference: TypeReference) -> Option<&TypeInfo> {
        self.types.info(reference)
    }

    /// The enumerators `name` names, by their own name or qualified by
    /// their enumeration's name, each with its qualified name, value, and
    /// enumeration, in identifier and then source order.
    pub(crate) fn enumerators_named(
        &self,
        name: &str,
    ) -> Vec<(String, crate::IntegerValue, TypeReference)> {
        let view = self.types.view();

        // An enumerator's own name is the whole name or what follows a
        // `::` in it.
        let mut candidates = view.with_enumerator(name).collect::<Vec<_>>();
        for (separator, _) in name.match_indices("::") {
            candidates.extend(view.with_enumerator(&name[separator + 2..]));
        }

        candidates.sort_unstable();
        candidates.dedup();
        let mut found = Vec::new();

        for id in candidates {
            let Some(info) = self.type_info(TypeReference { image: self.id, id }) else {
                continue;
            };

            let crate::TypeKind::Enumeration { enumerators, .. } = &info.kind else {
                continue;
            };

            for enumerator in enumerators.iter() {
                let qualified = format!("{}::{}", info.name, enumerator.name);
                if enumerator.name.as_ref() == name || qualified == name {
                    found.push((qualified, enumerator.value, info.reference));
                }
            }
        }

        found
    }

    /// The resolved types named exactly `name`, in identifier order.
    pub(crate) fn types_named_exactly<'a>(
        &'a self,
        name: &'a str,
    ) -> impl Iterator<Item = &'a TypeInfo> + 'a {
        self.types
            .view()
            .named(name)
            .filter_map(|id| self.type_info(TypeReference { image: self.id, id }))
    }

    fn type_names(&self) -> TypeNames<'_> {
        TypeNames(&self.types)
    }

    /// The types with exactly this language, path, and base, whatever their
    /// arguments, in identifier order: every instance of a template.
    #[must_use]
    pub fn type_instances(
        &self,
        language: SourceLanguage,
        path: &[&str],
        base: &str,
    ) -> Vec<TypeReference> {
        self.type_names()
            .instances(language, path, base, &*self.types)
    }

    /// The value of the integer constant the debug information declares as
    /// `name`, such as `runtime._Grunning`.
    #[must_use]
    pub fn constant(&self, name: &str) -> Option<crate::IntegerValue> {
        self.declarations().constant(name)
    }

    fn declarations(&self) -> crate::image::declarations::DeclarationView<'_> {
        crate::image::declarations::DeclarationView::new(&self.tables)
    }

    /// The Python runtime the module carries, if it carries one.
    #[must_use]
    pub fn python_runtime(&self) -> Option<crate::image::python::PythonRuntime> {
        crate::image::python::runtime(&self.tables)
    }

    /// The distinct producers of the image's debug information, such as
    /// `Go cmd/compile go1.27.1; regabi`.
    pub fn producers(&self) -> impl ExactSizeIterator<Item = &str> {
        self.declarations().producers()
    }

    /// What the coroutine of type `ty` is, or why its layout cannot be read
    /// as one; `None` for a type that is no coroutine.
    #[must_use]
    pub fn coroutine(
        &self,
        ty: TypeId,
    ) -> Option<std::result::Result<&crate::CoroutineInfo, &Arc<str>>> {
        self.types.coroutine(ty)
    }

    /// The functions that run the coroutine of type `ty`, or any type the
    /// same as it.
    #[must_use]
    pub fn coroutine_functions(&self, ty: TypeId) -> Vec<Function<'_>> {
        let class = |id| self.type_class(TypeReference { image: self.id, id });
        let index = self.coroutine_functions.get_or_init(|| {
            let mut index = HashMap::<u32, Vec<FunctionId>>::new();
            for function in self.functions() {
                if let Some(class) = function.coroutine().and_then(class) {
                    index.entry(class).or_default().push(function.id());
                }
            }

            index
        });

        class(ty)
            .and_then(|class| index.get(&class))
            .into_iter()
            .flatten()
            .filter_map(|id| self.function(*id))
            .collect()
    }

    /// Where the code instance `instance`, which runs a coroutine, goes for
    /// each state, or why that is unknown; `None` for an instance that runs
    /// none or is inlined.
    #[must_use]
    pub fn resume_points(
        &self,
        instance: CodeInstanceId,
    ) -> Option<std::result::Result<crate::ResumePoints, Arc<str>>> {
        crate::image::resumes::ResumeView::new(&self.tables).resume_points(instance)
    }

    /// Whether `address` is in a coroutine's dispatch on its state, or in
    /// the code leading from it into a state: code that runs on every
    /// resumption and is no statement of the program's.
    #[must_use]
    pub fn is_resume_code(&self, address: ImageAddress) -> bool {
        self.resume_code.containing(address).next().is_some()
    }

    /// The concrete type a Rust trait object's vtable at `address` is for.
    #[must_use]
    pub fn trait_object_type(&self, address: ImageAddress) -> Option<TypeReference> {
        let id = self.declarations().vtable(address)?;
        Some(TypeReference { image: self.id, id })
    }

    /// The C++ class whose vtable group, `vtable for X`, holds `address`:
    /// the class's name, and where the group begins.
    #[must_use]
    pub fn vtable_class(&self, address: ImageAddress) -> Option<(String, ImageAddress)> {
        let location = self.symbolize_data(address)?;
        let symbol = self.symbol(location.symbol)?;
        let name = crate::demangle::demangle(symbol.name())?;
        let class = name
            .strip_prefix("vtable for ")
            .or_else(|| name.strip_prefix("{vtable(")?.strip_suffix(")}"))?;

        Some((class.to_owned(), symbol.address()))
    }

    /// The type Go's runtime describes at `offset` from `runtime.types`, as
    /// its `DW_AT_go_runtime_type` says: the first in identifier order when
    /// several, such as a named type and its typedef, say so.
    #[must_use]
    pub fn go_runtime_type(&self, offset: u64) -> Option<TypeReference> {
        let id = self.types.view().go_runtime_type(offset)?;
        self.type_info(TypeReference { image: self.id, id })
            .map(|info| info.reference)
    }

    /// The types whose identity has this base, whatever their language,
    /// path, and arguments, in identifier order.
    #[must_use]
    pub fn types_with_base(&self, base: &str) -> Vec<TypeReference> {
        self.type_names().with_base(base)
    }

    /// The types a name could mean, in identifier order: those named
    /// exactly so, and those whose identity it spells. The name may omit
    /// outer path segments and trailing arguments, as in `vector<int>` for
    /// `std::vector<int, std::allocator<int> >`.
    #[must_use]
    pub fn types_named(&self, name: &str) -> Vec<TypeReference> {
        self.type_names().named(name, false, &*self.types)
    }

    /// Whether two of this image's types have the same identity, as one
    /// type defined in several units does.
    #[must_use]
    pub fn same_type(&self, left: TypeReference, right: TypeReference) -> bool {
        left == right
            || left.image == self.id
                && right.image == self.id
                && self
                    .types
                    .view()
                    .class(left.id)
                    .zip(self.types.view().class(right.id))
                    .is_some_and(|(left, right)| left == right)
    }

    /// Resolves a basename, canonical qualification, source qualification, or
    /// linkage identity to exactly one catalog entry.
    pub fn global_named(&self, selector: &str) -> Result<GlobalVariableInfo> {
        let matches = self
            .globals_by_selector
            .get_or_init(|| grouped_index(global_selectors(self)))
            .get(selector)
            .ok_or_else(|| Error::VariableNotFound(selector.to_owned()))?;
        let [id] = matches.as_ref() else {
            return Err(Error::AmbiguousGlobalVariable {
                selector: selector.to_owned(),
                candidates: matches
                    .iter()
                    .filter_map(|id| self.global(*id))
                    .map(|global| crate::GlobalVariableCandidate {
                        id: global.id,
                        qualified_name: Arc::clone(&global.qualified_name),
                        declaration_path: global
                            .declaration
                            .as_ref()
                            .and_then(|declaration| self.source_file(declaration.file))
                            .map(|source| Arc::clone(&source.path)),
                        declaration: global.declaration.clone(),
                    })
                    .collect(),
            });
        };

        self.global(*id)
            .ok_or_else(|| Error::internal("the global index names no catalog entry"))
    }

    /// Returns all source files referenced by this image.
    #[must_use]
    pub fn source_files(&self) -> &[SourceFile] {
        &self.source_files
    }

    /// Finds one source file using an absolute path or trailing path components.
    pub fn source_file_matching(&self, path: &Path) -> Result<&SourceFile> {
        let matches = self
            .source_files
            .iter()
            .filter(|source| path_matches(source.path.as_path(), path))
            .collect::<Vec<_>>();

        match matches.as_slice() {
            [source] => Ok(source),
            [] => Err(Error::SourceFileNotFound(path.to_path_buf())),
            _ => Err(Error::AmbiguousSourceFile {
                path: path.to_path_buf(),
                matches: matches
                    .iter()
                    .map(|source| source.path.as_ref().clone())
                    .collect(),
            }),
        }
    }

    /// The image's tables.
    pub(crate) const fn tables(&self) -> &Arc<crate::image::Image> {
        &self.tables
    }

    /// The bytes of the image's tables.
    #[cfg(test)]
    pub(crate) fn image_bytes(&self) -> &[u8] {
        self.tables.as_bytes()
    }

    /// The image's line tables.
    fn lines(&self) -> LineView<'_> {
        LineView::new(&self.tables)
    }

    /// Returns every ordered source line-program row in this image.
    pub fn statement_rows(&self) -> impl Iterator<Item = StatementRow> + '_ {
        self.lines().statement_rows()
    }

    /// Returns exact line-program control boundaries at an image address.
    ///
    /// Equal-address rows remain distinct and retain their sequence and
    /// ordinal. This query does not infer an epilogue region after an
    /// `epilogue_begin` marker.
    pub fn control_boundaries_at(
        &self,
        address: ImageAddress,
    ) -> impl Iterator<Item = StatementRow> + '_ {
        self.lines().control_boundaries_at(address)
    }

    /// Returns where a function breakpoint enters one code instance: every
    /// `prologue_end` address of an out-of-line instance, or otherwise the
    /// instance's own breakpoint entry.
    pub fn recommended_entries_for_instance(
        &self,
        instance: CodeInstanceId,
    ) -> impl Iterator<Item = BreakpointEntry> + '_ {
        self.code_instance(instance)
            .into_iter()
            .flat_map(CodeInstance::recommended_entries)
    }

    #[cfg(feature = "tools")]
    pub(crate) fn line_entries(&self) -> impl Iterator<Item = LineEntry> + '_ {
        self.lines().line_entries()
    }

    /// The line ranges that start in `instance`'s code, in order.
    pub(crate) fn line_entries_in(
        &self,
        instance: CodeInstance<'_>,
    ) -> impl Iterator<Item = LineEntry> + '_ {
        self.lines().line_entries_starting_in(instance.ranges())
    }

    pub(crate) fn line_entry_containing(&self, address: ImageAddress) -> Option<LineEntry> {
        self.lines().line_entry_containing(address)
    }

    /// Looks up a concrete code instance by identifier.
    #[must_use]
    pub fn code_instance(&self, id: CodeInstanceId) -> Option<CodeInstance<'_>> {
        self.function_view().instance(id)
    }

    /// Returns the concrete instances of one source-level function.
    pub fn instances_for_function(
        &self,
        function: FunctionId,
    ) -> impl Iterator<Item = CodeInstance<'_>> {
        self.function(function)
            .into_iter()
            .flat_map(Function::instances)
    }

    /// The instances whose code contains `address`.
    pub(crate) fn instances_containing(
        &self,
        address: ImageAddress,
    ) -> impl Iterator<Item = CodeInstance<'_>> {
        self.function_view().instances_containing(address)
    }

    /// The physical instance whose code contains `address`, the first of
    /// several.
    fn physical_instance(&self, address: ImageAddress) -> Option<CodeInstance<'_>> {
        self.instances_containing(address)
            .filter(|instance| instance.is_out_of_line())
            .min_by_key(|instance| instance.id())
    }

    /// Returns image addresses associated with one source line.
    pub fn statement_addresses(
        &self,
        file: SourceFileId,
        line: LineNumber,
    ) -> impl Iterator<Item = ImageAddress> + use<> {
        let lines = self.lines();
        let mut addresses = lines
            .statements(file, line.get()..=line.get())
            .iter()
            .map(|key| lines.address(key.row.get()))
            .collect::<Vec<_>>();
        addresses.sort_unstable();
        addresses.dedup();
        addresses.into_iter()
    }

    /// Returns the lines of one source file within `lines` that have
    /// statement addresses: the lines a source breakpoint stops at as
    /// requested.
    pub fn breakpoint_lines(
        &self,
        file: SourceFileId,
        lines: std::ops::RangeInclusive<LineNumber>,
    ) -> impl Iterator<Item = LineNumber> + '_ {
        let mut previous = None;
        self.lines()
            .statements(file, lines.start().get()..=lines.end().get())
            .iter()
            .filter_map(move |key| {
                let line = key.line.get();
                (previous.replace(line) != Some(line)).then(|| LineNumber::new(line))?
            })
    }

    /// The first line of `file` at or after `line` with statements.
    pub(crate) fn next_statement_line(
        &self,
        file: SourceFileId,
        line: LineNumber,
    ) -> Option<LineNumber> {
        let key = self
            .lines()
            .statements(file, line.get()..=u64::MAX)
            .first()?;
        LineNumber::new(key.line.get())
    }

    /// The last line of `file` at or before `line` with statements.
    pub(crate) fn previous_statement_line(
        &self,
        file: SourceFileId,
        line: u64,
    ) -> Option<LineNumber> {
        let key = self.lines().statements(file, 1..=line).last()?;
        LineNumber::new(key.line.get())
    }

    /// Finds the line a source breakpoint requested at `line` stops at, as
    /// gdb does: the line itself when it has statements, otherwise the next
    /// line that does, provided a function whose statements begin at or
    /// before the request contains it. A line between functions never moves
    /// into the next one, and a line of a Go file never moves at all.
    #[must_use]
    pub fn breakpoint_line(&self, file: SourceFileId, line: LineNumber) -> Option<LineNumber> {
        let next = self.next_statement_line(file, line)?;
        if next == line {
            return Some(line);
        }

        if self.keeps_line_breakpoints(file) {
            return None;
        }

        let lines = self.lines();
        let before = lines.statements(file, 1..=line.get());
        let encloses_request = |instance: CodeInstance<'_>| {
            before
                .iter()
                .any(|key| instance.contains(lines.address(key.row.get())))
        };

        self.statement_addresses(file, next)
            .flat_map(|address| self.instances_containing(address))
            .filter(|instance| instance.is_out_of_line())
            .any(encloses_request)
            .then_some(next)
    }

    /// Finds the single function with the supplied source-level name.
    ///
    /// Only functions with code compete: a compile unit that merely calls
    /// a function defined in another one may describe it by a declaration.
    pub fn function_named(&self, name: &str) -> Result<Function<'_>> {
        let named = self.functions_named(name).collect::<Vec<_>>();
        let defined = named
            .iter()
            .copied()
            .filter(|function| function.instances().next().is_some())
            .collect::<Vec<_>>();

        match (defined.as_slice(), named.as_slice()) {
            ([function], _) | ([], [function]) => Ok(*function),
            (_, []) => Err(Error::FunctionNotFound(name.to_owned())),
            _ => Err(Error::DuplicateFunction(name.to_owned())),
        }
    }

    /// Returns every function with the supplied source-level name, such as
    /// C++ overloads and same-named static functions of different files.
    pub fn functions_named<'a>(&'a self, name: &str) -> impl Iterator<Item = Function<'a>> + 'a {
        self.function_view().named(name)
    }

    /// The functions whose own name is `name`, in identifier order: unlike
    /// [`Self::functions_named`], `main` names only functions called that.
    pub fn functions_called<'a>(&'a self, name: &str) -> impl Iterator<Item = Function<'a>> + 'a {
        self.function_view().called(name)
    }

    /// Where each out-of-line instance of a function that begins a panic
    /// begins, in no particular order.
    pub fn panic_entries(&self) -> &[ImageAddress] {
        self.panic_entries.get_or_init(|| {
            self.functions()
                .filter(|function| function.role() == CodeRole::Panic)
                .flat_map(|function| self.instances_for_function(function.id()))
                .filter(|instance| matches!(instance.kind(), CodeInstanceKind::OutOfLine))
                .filter_map(|instance| instance.ranges().next())
                .map(|range| range.start)
                .collect()
        })
    }

    /// Every linker symbol with the supplied name.
    pub fn symbols_named<'a>(&'a self, name: &str) -> impl Iterator<Item = Symbol<'a>> + 'a {
        self.symbol_view().named(name)
    }

    /// Every symbol that answers to a name as [`Symbol::answers_to`]
    /// reads it.
    pub fn symbols_answering<'a>(&'a self, name: &'a str) -> impl Iterator<Item = Symbol<'a>> {
        let index = self.symbols_by_last_part.get_or_init(|| {
            let mut index = HashMap::<Box<str>, Vec<SymbolId>>::new();
            for symbol in self.symbols() {
                let demangled = crate::demangle::demangle(symbol.name());
                let parts = [
                    Some(symbol.name()),
                    Some(symbol.unversioned_name()),
                    demangled.as_deref().map(crate::demangle::last_part),
                ];
                for part in parts.into_iter().flatten() {
                    let ids = index.entry(part.into()).or_default();
                    if ids.last() != Some(&symbol.id()) {
                        ids.push(symbol.id());
                    }
                }
            }

            index
        });

        let mut candidates = [name, crate::demangle::last_part(name)]
            .into_iter()
            .filter_map(|part| index.get(part))
            .flatten()
            .copied()
            .collect::<Vec<_>>();
        candidates.sort_unstable();
        candidates.dedup();
        candidates
            .into_iter()
            .filter_map(|id| self.symbol(id))
            .filter(move |symbol| symbol.answers_to(name))
    }

    /// The GOT slots the loader fills with functions' addresses.
    #[must_use]
    pub fn got_slots(&self) -> &[GotSlot] {
        &self.got_slots
    }

    /// Whether the module imports a function of the name from another, so
    /// that a module with its code is yet to load.
    #[must_use]
    pub fn imports_function(&self, name: &str) -> bool {
        self.got_slots.iter().any(
            |slot| matches!(&slot.target, crate::GotTarget::Import(import) if &**import == name || crate::demangle::spells(import, name)),
        )
    }

    /// Finds the single linker symbol with the supplied name.
    pub fn symbol_named(&self, name: &str) -> Result<Symbol<'_>> {
        let mut matches = self.symbols_named(name);
        let symbol = matches
            .next()
            .ok_or_else(|| Error::SymbolNotFound(name.to_owned()))?;
        if matches.next().is_some() {
            return Err(Error::DuplicateSymbol(name.to_owned()));
        }

        Ok(symbol)
    }

    /// What the code at an image address is to unwinding and stepping: the
    /// role of the physical function containing it, or else of the code
    /// symbol naming it, or else ordinary code.
    #[must_use]
    pub fn code_role(&self, address: ImageAddress) -> CodeRole {
        let physical = self.physical_instance(address);
        if let Some(function) = physical.and_then(|instance| self.function(instance.function())) {
            return function.role();
        }

        self.symbolize(address)
            .and_then(|location| self.symbol(location.symbol))
            .map_or(CodeRole::Ordinary, Symbol::role)
    }

    /// Resolves an image address to its available function and source metadata.
    #[must_use]
    pub fn locate(&self, address: ImageAddress) -> ImageLocation {
        let physical = self.physical_instance(address);
        let inline_frames = self.inline_frames(address, physical.map(CodeInstance::id));
        let logical_instance = match &inline_frames {
            InlineFrameLookup::Unique(chain) => chain.instances.last().copied(),
            InlineFrameLookup::None | InlineFrameLookup::Ambiguous(_) => None,
        };

        let function_id = logical_instance
            .and_then(|instance| self.code_instance(instance))
            .or(physical)
            .map(CodeInstance::function);
        let function = function_id
            .and_then(|function_id| self.function(function_id))
            .map(Function::info);
        let source = self
            .line_entry_containing(address)
            .map(|entry| entry.location);

        ImageLocation {
            address,
            function,
            physical_instance: physical.map(CodeInstance::id),
            inline_frames,
            source,
            symbol: self.symbolize(address),
        }
    }

    fn inline_frames(
        &self,
        address: ImageAddress,
        physical: Option<CodeInstanceId>,
    ) -> InlineFrameLookup {
        let mut chains = Vec::new();

        for instance in self.instances_containing(address).filter(|instance| {
            matches!(
                instance.kind(),
                CodeInstanceKind::Inline { call_site: Some(_) }
            )
        }) {
            if let Some(chain) = self.inline_chain(instance.id(), address, physical)
                && !chains.contains(&chain)
            {
                chains.push(chain);
            }
        }

        let chains: Vec<_> = chains
            .iter()
            .filter(|candidate| {
                !chains.iter().any(|other| {
                    candidate.len() < other.len() && other.starts_with(candidate.as_slice())
                })
            })
            .cloned()
            .map(|instances| InlineChain {
                instances: instances.into(),
            })
            .collect();

        match <[InlineChain; 1]>::try_from(chains) {
            Ok([chain]) => InlineFrameLookup::Unique(chain),
            Err(chains) if chains.is_empty() => InlineFrameLookup::None,
            Err(chains) => InlineFrameLookup::Ambiguous(chains.into()),
        }
    }

    fn inline_chain(
        &self,
        mut instance: CodeInstanceId,
        address: ImageAddress,
        physical: Option<CodeInstanceId>,
    ) -> Option<Vec<CodeInstanceId>> {
        let mut chain = Vec::new();

        loop {
            let current = self.code_instance(instance)?;

            if !current.contains(address) {
                return None;
            }

            match current.kind() {
                CodeInstanceKind::Inline { call_site: Some(_) } => chain.push(current.id()),
                CodeInstanceKind::Inline { call_site: None } => return None,
                CodeInstanceKind::OutOfLine => {
                    if Some(current.id()) != physical {
                        return None;
                    }

                    break;
                }
            }

            instance = current.parent()?;
        }

        chain.reverse();
        Some(chain)
    }

    /// Looks up a source file by its identifier.
    #[must_use]
    pub fn source_file(&self, id: SourceFileId) -> Option<&SourceFile> {
        self.source_files.get(id.index())
    }

    /// Every named integer constant, for a dump of every answer.
    #[cfg(feature = "tools")]
    pub(crate) fn constants_for_dump(&self) -> impl Iterator<Item = (&str, crate::IntegerValue)> {
        self.declarations().constants()
    }

    /// Every thread-local variable, for a dump of every answer.
    #[cfg(feature = "tools")]
    pub(crate) fn thread_locals_for_dump(
        &self,
    ) -> impl Iterator<Item = (&str, std::result::Result<ThreadLocal, Arc<str>>)> {
        self.facts().thread_locals()
    }

    /// Every Rust vtable, for a dump of every answer.
    #[cfg(feature = "tools")]
    pub(crate) fn vtables_for_dump(&self) -> impl Iterator<Item = (ImageAddress, TypeReference)> {
        let image = self.id;
        self.declarations()
            .vtables()
            .map(move |(address, id)| (address, TypeReference { image, id }))
    }

    /// Every Go runtime type descriptor offset a type names, for a dump of
    /// every answer.
    #[cfg(feature = "tools")]
    pub(crate) fn go_runtime_type_offsets_for_dump(&self) -> Vec<u64> {
        self.types
            .view()
            .go_runtime_types()
            .map(|(offset, _)| offset)
            .collect()
    }
}

/// Collects the addresses debug information and code symbols prove begin
/// instructions, keeping the strongest evidence for each address.
fn instruction_starts(
    metadata: &ModuleMetadata,
    code_instances: &[CodeInstanceInfo],
) -> Vec<(ImageAddress, crate::BoundaryEvidence)> {
    let mut starts = BTreeMap::new();
    let functions = code_instances
        .iter()
        .filter(|instance| matches!(instance.kind, CodeInstanceKind::OutOfLine))
        .flat_map(|instance| instance.ranges.iter())
        .map(|range| (range.start, crate::BoundaryEvidence::FunctionRange));
    let symbols = metadata
        .symbols
        .iter()
        .filter_map(|symbol| symbol.extent)
        .map(|extent| (extent.range.start, crate::BoundaryEvidence::CodeSymbol));
    let sections = metadata
        .sections
        .iter()
        .filter(|section| section.executable)
        .map(|section| (section.range.start, crate::BoundaryEvidence::SectionStart));

    for (address, evidence) in functions.chain(symbols).chain(sections) {
        starts
            .entry(address)
            .and_modify(|current: &mut crate::BoundaryEvidence| *current = (*current).min(evidence))
            .or_insert(evidence);
    }

    starts.into_iter().collect()
}

/// Matches an absolute path exactly and a relative path as a suffix of whole
/// components, ignoring `.` components such as a leading `./`.
fn path_matches(candidate: &Path, requested: &Path) -> bool {
    // Last component first, which tells most files apart at once.
    fn significant(path: &Path) -> impl Iterator<Item = std::path::Component<'_>> {
        path.components()
            .rev()
            .filter(|component| *component != std::path::Component::CurDir)
    }

    if requested.is_absolute() {
        return candidate == requested;
    }

    let mut candidate = significant(candidate);
    let mut requested = significant(requested).peekable();

    requested.peek().is_some() && requested.all(|component| candidate.next() == Some(component))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EntryProvenance;
    use crate::model::{
        Architecture, BaseType, BaseTypeEncoding, ByteOrder, GlobalVariableType,
        GlobalVariableVisibility, LineSequenceId, PointerWidth, StatementFlags, SymbolBinding,
        SymbolExtent, TypeId, TypeKind,
    };

    fn test_image(end: u64, metadata: ModuleMetadata) -> ModuleImage {
        ModuleImage::new(
            PathBuf::from("/test"),
            TargetDescription {
                architecture: Architecture::X86_64,
                byte_order: ByteOrder::Little,
                pointer_width: PointerWidth::Bits64,
            },
            AddressRange {
                start: ImageAddress::new(0),
                end: ImageAddress::new(end),
            },
            metadata,
        )
    }

    fn files(paths: &[&str]) -> crate::image::lines::Files {
        let mut files = crate::image::lines::Files::default();
        for path in paths {
            files.intern(PathBuf::from(path));
        }

        files
    }

    fn functions(names: &[&str]) -> Vec<FunctionInfo> {
        names
            .iter()
            .enumerate()
            .map(|(id, name)| FunctionInfo {
                id: FunctionId::new(u32::try_from(id).expect("small function count")),
                name: (*name).into(),
                linkage_name: None,
                declaration: None,
                language: crate::SourceLanguage::C,
                role: CodeRole::Ordinary,
                enclosing: None,
                coroutine: None,
                generics: std::sync::Arc::from([]),
                main_subprogram: false,
            })
            .collect()
    }

    /// An image of two globals named `shared`, in two files: the first of
    /// type `int`, the second of a type that could not be read.
    fn two_globals() -> (ModuleImage, TypeInfo) {
        use crate::image::variables::{
            DataObject, Global, Metadata, MetadataAbsence, TypeResolution, Variables,
        };

        let int = TypeInfo {
            reference: TypeReference {
                image: ModuleImageId::new(0),
                id: TypeId::new(0),
            },
            name: "int".into(),
            byte_size: Some(4),
            kind: TypeKind::Base(BaseType {
                name: "int".into(),
                base_name: "int".into(),
                encoding: BaseTypeEncoding::Signed,
                byte_size: 4,
                bit_size: None,
            }),
            identity: None,
        };

        let object = |file, type_info| DataObject {
            debug_info_offset: None,
            kind: crate::VariableKind::Global,
            name: "shared".into(),
            declaration: Some(SourceLocation {
                file: SourceFileId::new(file),
                line: LineNumber::new(7).expect("nonzero line"),
                column: None,
            }),
            ranges: Arc::from([]),
            go_declaration: None,
            instance: None,
            lexical_depth: 0,
            order: u64::from(file),
            type_info,
            escaped: None,
            hidden: false,
            coroutine: None,
            value: Metadata::Absent(MetadataAbsence::NoLocation),
            frame_base: Metadata::Absent(MetadataAbsence::NotApplicable),
            malformed: None,
        };

        let global = |object, qualified_name: &str, linkage_name: &str| Global {
            object,
            qualified_name: qualified_name.into(),
            linkage_name: Some(linkage_name.into()),
            external: object == 1,
        };

        let image = test_image(
            1,
            ModuleMetadata {
                types: Arc::from([TypeNode::Resolved(int.clone())]),
                variables: Variables {
                    objects: vec![
                        object(0, TypeResolution::Resolved(TypeId::new(0))),
                        object(1, TypeResolution::Malformed("no type".into())),
                    ],
                    globals: vec![
                        global(0, "left::shared", "_ZL11left_shared"),
                        global(1, "right::shared", "_ZL12right_shared"),
                    ],
                    ..Variables::default()
                },
                files: files(&["/build/src/left.c", "/build/src/right.c"]),
                ..ModuleMetadata::default()
            },
        );

        (image, int)
    }

    /// Globals answer to their names, qualified names, linkage names, and
    /// qualified names after their files, and read their types and
    /// visibility from the image.
    #[test]
    fn global_indexes_support_exact_qualification_and_structured_ambiguity() {
        let (image, int) = two_globals();
        let [left, right] = [0, 1].map(|id| {
            image
                .global(GlobalVariableId::new(id))
                .expect("the global exists")
        });
        assert_eq!(left.type_info, GlobalVariableType::Resolved(int));
        assert_eq!(
            right.type_info,
            GlobalVariableType::Malformed(crate::VariableMalformedReason {
                kind: crate::VariableMalformedKind::InvalidTypeGraph,
                description: "no type".into(),
            })
        );
        assert_eq!(
            [left.visibility, right.visibility],
            [
                GlobalVariableVisibility::CompilationUnit,
                GlobalVariableVisibility::External
            ]
        );
        assert!(image.global(GlobalVariableId::new(2)).is_none());
        assert_eq!(image.globals().count(), 2);
        let selected = |selector| image.global_named(selector).expect(selector).id;
        assert_eq!(selected("left::shared"), GlobalVariableId::new(0));
        assert_eq!(selected("right.c::right::shared"), GlobalVariableId::new(1));
        assert_eq!(selected("_ZL11left_shared"), GlobalVariableId::new(0));
        let Err(Error::AmbiguousGlobalVariable {
            selector,
            candidates,
        }) = image.global_named("shared")
        else {
            panic!("the basename is not ambiguous");
        };

        assert_eq!(selector, "shared");
        assert_eq!(
            candidates
                .iter()
                .map(|candidate| (candidate.id, candidate.qualified_name.as_ref()))
                .collect::<Vec<_>>(),
            [
                (GlobalVariableId::new(0), "left::shared"),
                (GlobalVariableId::new(1), "right::shared"),
            ]
        );
    }

    #[test]
    fn module_type_graph_resolves_only_owned_dense_references() {
        let image_id = ModuleImageId::new(7);
        let reference = |image, id| TypeReference {
            image,
            id: TypeId::new(id),
        };

        let image = test_image(
            1,
            ModuleMetadata {
                types: Arc::from([
                    TypeNode::Resolved(TypeInfo {
                        reference: reference(image_id, 0),
                        name: "int".into(),
                        byte_size: Some(4),
                        kind: TypeKind::Opaque {
                            description: "test type".into(),
                        },
                        identity: None,
                    }),
                    TypeNode::Malformed {
                        reference: reference(image_id, 1),
                        description: "bad type".into(),
                    },
                ]),
                ..ModuleMetadata::default()
            },
        );
        let image = ModuleImage::bind(
            &Binding {
                path: image.path_arc(),
                debug_path: None,
                id: image_id,
            },
            Arc::clone(image.tables()),
        )
        .unwrap();

        assert_eq!(
            image
                .type_info(reference(image_id, 0))
                .map(|info| &*info.name),
            Some("int")
        );
        assert!(matches!(
            image.type_node(reference(image_id, 1)),
            Some(TypeNode::Malformed { description, .. }) if &**description == "bad type"
        ));
        assert!(image.type_info(reference(image_id, 1)).is_none());
        assert!(
            image
                .type_node(reference(ModuleImageId::new(8), 0))
                .is_none()
        );
        assert!(image.type_node(reference(image_id, 2)).is_none());
    }

    #[test]
    fn source_path_matching_uses_whole_trailing_components() {
        let candidate = Path::new("/build/project/src/main.c");
        assert!(path_matches(candidate, Path::new("main.c")));
        assert!(path_matches(candidate, Path::new("src/main.c")));
        assert!(path_matches(candidate, Path::new("./main.c")));
        assert!(path_matches(candidate, Path::new("src/./main.c")));
        assert!(!path_matches(candidate, Path::new(".")));
        assert!(path_matches(candidate, candidate));
        assert!(!path_matches(candidate, Path::new("rc/main.c")));
        assert!(!path_matches(candidate, Path::new("other/main.c")));
    }

    fn source(line: u64) -> SourceLocation {
        SourceLocation {
            file: SourceFileId::new(0),
            line: LineNumber::new(line).expect("nonzero line"),
            column: None,
        }
    }

    fn instance(
        id: u32,
        function: u32,
        parent: Option<u32>,
        kind: CodeInstanceKind,
        ranges: &[(u64, u64)],
    ) -> CodeInstanceInfo {
        CodeInstanceInfo {
            id: CodeInstanceId::new(id),
            function: FunctionId::new(function),
            parent: parent.map(CodeInstanceId::new),
            kind,
            ranges: ranges
                .iter()
                .map(|&(start, end)| AddressRange {
                    start: ImageAddress::new(start),
                    end: ImageAddress::new(end),
                })
                .collect::<Vec<_>>()
                .into(),
            breakpoint_entry: None,
        }
    }

    fn inline_test_image(code_instances: Vec<CodeInstanceInfo>) -> ModuleImage {
        test_image(
            100,
            ModuleMetadata {
                functions: functions(&["physical", "middle", "leaf", "sibling"]),
                code_instances,
                // The file the call sites name.
                files: files(&["/build/src/main.c"]),
                ..ModuleMetadata::default()
            },
        )
    }

    #[test]
    fn instruction_starts_come_from_out_of_line_ranges_code_symbols_and_code_sections() {
        use crate::BoundaryEvidence::{CodeSymbol, FunctionRange, SectionStart};

        let code_symbol = |id, name: &str, start, end| SymbolInfo {
            id: SymbolId::new(id),
            name: name.into(),
            address: ImageAddress::new(start),
            kind: SymbolKind::Function,
            binding: SymbolBinding::Global,
            exported: true,
            extent: Some(SymbolExtent {
                range: AddressRange {
                    start: ImageAddress::new(start),
                    end: ImageAddress::new(end),
                },
                provenance: SymbolExtentProvenance::Declared,
            }),
            storage: None,
            role: CodeRole::Ordinary,
        };

        let section = |id, name: &str, start, end, executable| SectionInfo {
            id: SectionId::new(id),
            name: name.into(),
            range: AddressRange {
                start: ImageAddress::new(start),
                end: ImageAddress::new(end),
            },
            executable,
            writable: !executable,
        };

        let image = test_image(
            0x100,
            ModuleMetadata {
                functions: functions(&["split"]),
                code_instances: vec![
                    // A function split into two ranges, and an inline
                    // expansion whose start proves nothing on its own.
                    instance(
                        0,
                        0,
                        None,
                        CodeInstanceKind::OutOfLine,
                        &[(0x20, 0x30), (0x60, 0x68)],
                    ),
                    instance(
                        1,
                        0,
                        Some(0),
                        CodeInstanceKind::Inline { call_site: None },
                        &[(0x24, 0x28)],
                    ),
                ],
                // A symbol at a function's start is weaker evidence than
                // the function itself.
                symbols: vec![
                    code_symbol(0, "split", 0x20, 0x30),
                    code_symbol(1, "symbol_only", 0x40, 0x48),
                ],
                sections: vec![
                    section(0, ".text", 0x10, 0x70, true),
                    section(1, ".data", 0x80, 0x90, false),
                ],
                ..ModuleMetadata::default()
            },
        );
        let starts = |start, end| {
            image
                .instruction_starts(AddressRange {
                    start: ImageAddress::new(start),
                    end: ImageAddress::new(end),
                })
                .map(|(address, evidence)| (address.get(), evidence))
                .collect::<Vec<_>>()
        };

        assert_eq!(
            starts(0, 0x100),
            [
                (0x10, SectionStart),
                (0x20, FunctionRange),
                (0x40, CodeSymbol),
                (0x60, FunctionRange),
            ]
        );
        assert_eq!(
            starts(0x20, 0x60),
            [(0x20, FunctionRange), (0x40, CodeSymbol)]
        );
    }

    #[test]
    fn control_boundaries_keep_exact_rows_and_enter_physical_instances_after_prologues() {
        let row = |address, location, flags, sequence, ordinal| StatementRow {
            address: ImageAddress::new(address),
            operation_index: 0,
            location,
            discriminator: 0,
            flags,
            isa: 0,
            sequence: LineSequenceId::new(sequence),
            ordinal,
        };

        let entry = |address, provenance| BreakpointEntry {
            address: ImageAddress::new(address),
            provenance,
        };

        let mut physical = instance(0, 0, None, CodeInstanceKind::OutOfLine, &[(0x10, 0x30)]);
        physical.breakpoint_entry = Some(entry(0x10, EntryProvenance::Explicit));
        let mut inline = instance(
            1,
            1,
            Some(0),
            CodeInstanceKind::Inline {
                call_site: Some(source(7)),
            },
            &[(0x14, 0x20)],
        );
        inline.breakpoint_entry = Some(entry(0x14, EntryProvenance::RangeStart));
        let flags = StatementFlags::empty;
        let image = test_image(
            0x40,
            ModuleMetadata {
                functions: functions(&["physical", "inline"]),
                code_instances: vec![physical, inline],
                files: files(&["/build/src/main.c"]),
                lines: crate::image::lines::from_statement_rows(&[
                    row(
                        0x14,
                        None,
                        flags().with_statement(true).with_prologue_end(true),
                        0,
                        2,
                    ),
                    row(
                        0x14,
                        Some(source(8)),
                        flags().with_epilogue_begin(true),
                        0,
                        3,
                    ),
                    row(0x18, Some(source(9)), flags().with_prologue_end(true), 1, 0),
                    row(0x20, None, flags().with_epilogue_begin(true), 1, 1),
                ]),
                ..ModuleMetadata::default()
            },
        );

        let exact = image
            .control_boundaries_at(ImageAddress::new(0x14))
            .map(|row| (row.sequence, row.ordinal, row.location.is_some()))
            .collect::<Vec<_>>();
        assert_eq!(
            exact,
            [
                (LineSequenceId::new(0), 2, false),
                (LineSequenceId::new(0), 3, true)
            ]
        );
        assert!(
            image
                .control_boundaries_at(ImageAddress::new(0x15))
                .next()
                .is_none()
        );
        let entries = |id| {
            image
                .recommended_entries_for_instance(CodeInstanceId::new(id))
                .collect::<Vec<_>>()
        };

        assert_eq!(
            entries(0),
            [
                entry(0x14, EntryProvenance::Statement),
                entry(0x18, EntryProvenance::Statement)
            ]
        );

        // Inline instances have no prologue of their own.
        assert_eq!(entries(1), [entry(0x14, EntryProvenance::RangeStart)]);
        assert!(
            image
                .statement_addresses(SourceFileId::new(0), LineNumber::new(8).unwrap())
                .next()
                .is_none()
        );
    }

    #[test]
    fn inline_lookup_preserves_nested_discontiguous_ranges_and_boundaries() {
        let image = inline_test_image(vec![
            instance(0, 0, None, CodeInstanceKind::OutOfLine, &[(0, 100)]),
            instance(
                1,
                1,
                Some(0),
                CodeInstanceKind::Inline {
                    call_site: Some(source(10)),
                },
                &[(20, 30), (40, 50)],
            ),
            instance(
                2,
                2,
                Some(1),
                CodeInstanceKind::Inline {
                    call_site: Some(source(20)),
                },
                &[(22, 25)],
            ),
            instance(
                3,
                3,
                Some(0),
                CodeInstanceKind::Inline { call_site: None },
                &[(60, 70)],
            ),
        ]);

        let InlineFrameLookup::Unique(nested) = image.locate(ImageAddress::new(22)).inline_frames
        else {
            panic!("nested inline chain was not unique")
        };

        assert_eq!(
            nested.instances.as_ref(),
            &[CodeInstanceId::new(1), CodeInstanceId::new(2)]
        );
        assert!(matches!(
            image.locate(ImageAddress::new(40)).inline_frames,
            InlineFrameLookup::Unique(_)
        ));
        for address in [30, 35, 50, 60] {
            assert_eq!(
                image.locate(ImageAddress::new(address)).inline_frames,
                InlineFrameLookup::None,
                "unexpected inline frame at {address}"
            );
        }
    }

    #[test]
    fn overlapping_sibling_inline_instances_are_explicitly_ambiguous() {
        let image = inline_test_image(vec![
            instance(0, 0, None, CodeInstanceKind::OutOfLine, &[(0, 100)]),
            instance(
                1,
                1,
                Some(0),
                CodeInstanceKind::Inline {
                    call_site: Some(source(10)),
                },
                &[(20, 30)],
            ),
            instance(
                2,
                2,
                Some(0),
                CodeInstanceKind::Inline {
                    call_site: Some(source(11)),
                },
                &[(25, 35)],
            ),
        ]);

        let InlineFrameLookup::Ambiguous(chains) =
            image.locate(ImageAddress::new(26)).inline_frames
        else {
            panic!("overlapping siblings were not reported as ambiguous")
        };

        let mut chains = chains
            .iter()
            .map(|chain| chain.instances.as_ref())
            .collect::<Vec<_>>();
        chains.sort_unstable();
        assert_eq!(chains, [[CodeInstanceId::new(1)], [CodeInstanceId::new(2)]]);
    }

    /// One test symbol: name, start, end (equal for a symbol without an
    /// extent), provenance, kind, binding, and whether it is exported.
    type TestSymbol = (
        &'static str,
        u64,
        u64,
        SymbolExtentProvenance,
        SymbolKind,
        SymbolBinding,
        bool,
    );

    fn symbol_test_image(symbols: &[TestSymbol]) -> ModuleImage {
        sectioned_symbol_test_image(symbols, &[])
    }

    /// Builds an image of symbols and sections, each section given by name,
    /// start, end, and whether it is executable.
    fn sectioned_symbol_test_image(
        symbols: &[TestSymbol],
        sections: &[(&'static str, u64, u64, bool)],
    ) -> ModuleImage {
        let sections = sections
            .iter()
            .enumerate()
            .map(|(index, &(name, start, end, executable))| SectionInfo {
                id: SectionId::new(u32::try_from(index).expect("test section count")),
                name: name.into(),
                range: AddressRange {
                    start: ImageAddress::new(start),
                    end: ImageAddress::new(end),
                },
                executable,
                writable: !executable,
            })
            .collect();
        let symbols = symbols
            .iter()
            .enumerate()
            .map(
                |(index, &(name, start, end, provenance, kind, binding, exported))| SymbolInfo {
                    id: SymbolId::new(u32::try_from(index).expect("test symbol count")),
                    name: name.into(),
                    address: ImageAddress::new(start),
                    kind,
                    binding,
                    exported,
                    extent: (start < end && kind != SymbolKind::Data).then_some(SymbolExtent {
                        range: AddressRange {
                            start: ImageAddress::new(start),
                            end: ImageAddress::new(end),
                        },
                        provenance,
                    }),
                    storage: (kind == SymbolKind::Data).then_some(AddressRange {
                        start: ImageAddress::new(start),
                        end: ImageAddress::new(end),
                    }),
                    role: CodeRole::Ordinary,
                },
            )
            .collect();
        test_image(
            0x1000,
            ModuleMetadata {
                symbols,
                sections,
                ..ModuleMetadata::default()
            },
        )
    }

    fn symbolized(image: &ModuleImage, address: u64) -> Option<(&str, u64)> {
        image.symbolize(ImageAddress::new(address)).map(|location| {
            (
                image.symbol(location.symbol).expect("known").name(),
                location.offset,
            )
        })
    }

    #[test]
    fn symbolization_uses_only_containing_extents_and_prefers_declared_innermost_code() {
        use SymbolBinding::{Global, Local};
        use SymbolExtentProvenance::{Declared, Inferred};
        use SymbolKind::{Data, Function};

        let image = symbol_test_image(&[
            ("outer", 0x100, 0x200, Declared, Function, Global, true),
            ("inner", 0x140, 0x160, Declared, Function, Local, false),
            // An unsized label inside a sized function never displaces it.
            ("label", 0x180, 0x190, Inferred, Function, Global, true),
            ("tail", 0x300, 0x340, Inferred, Function, Local, false),
            // Symbols without an extent never name code.
            ("object", 0x240, 0x240, Declared, Data, Global, true),
        ]);

        assert_eq!(symbolized(&image, 0xff), None);
        assert_eq!(symbolized(&image, 0x100), Some(("outer", 0)));
        assert_eq!(symbolized(&image, 0x13f), Some(("outer", 0x3f)));
        assert_eq!(symbolized(&image, 0x140), Some(("inner", 0)));
        assert_eq!(symbolized(&image, 0x15f), Some(("inner", 0x1f)));
        assert_eq!(symbolized(&image, 0x160), Some(("outer", 0x60)));
        assert_eq!(symbolized(&image, 0x185), Some(("outer", 0x85)));
        assert_eq!(symbolized(&image, 0x1ff), Some(("outer", 0xff)));

        // No nearest preceding symbol is guessed for unnamed code.
        assert_eq!(symbolized(&image, 0x200), None);
        assert_eq!(symbolized(&image, 0x240), None);
        assert_eq!(symbolized(&image, 0x33f), Some(("tail", 0x3f)));
        assert_eq!(
            image
                .symbolize(ImageAddress::new(0x300))
                .map(|location| location.provenance),
            Some(Inferred)
        );
        assert_eq!(symbolized(&image, 0x340), None);
    }

    #[test]
    fn same_extent_aliases_resolve_by_kind_binding_export_and_spelling() {
        use SymbolBinding::{Global, Local, Weak};
        use SymbolExtentProvenance::Declared;
        use SymbolKind::{Function, IndirectFunction};

        // Each row adds a candidate that outranks every earlier one by
        // exactly one rule and sorts after them by name, so only that rule
        // can select it.
        let ladder: [TestSymbol; 6] = [
            (
                "_a_resolver",
                0x10,
                0x20,
                Declared,
                IndirectFunction,
                Global,
                true,
            ),
            ("_b_local", 0x10, 0x20, Declared, Function, Local, false),
            ("_c_weak", 0x10, 0x20, Declared, Function, Weak, false),
            ("_d_hidden", 0x10, 0x20, Declared, Function, Global, false),
            ("_e_exported", 0x10, 0x20, Declared, Function, Global, true),
            ("f_exported", 0x10, 0x20, Declared, Function, Global, true),
        ];

        for count in 1..=ladder.len() {
            let image = symbol_test_image(&ladder[..count]);
            assert_eq!(
                symbolized(&image, 0x18).map(|(name, _)| name),
                Some(ladder[count - 1].0),
                "{count} candidates"
            );
        }

        // With every rule tied, the bytewise-smallest name wins regardless
        // of catalog order.
        let tied = symbol_test_image(&[
            ("beta", 0x10, 0x20, Declared, Function, Global, true),
            ("alpha", 0x10, 0x20, Declared, Function, Global, true),
        ]);
        assert_eq!(symbolized(&tied, 0x10), Some(("alpha", 0)));
    }

    #[test]
    fn descriptions_prefer_code_then_declared_storage_and_never_guess_a_neighbor() {
        use SymbolBinding::{Global, Local};
        use SymbolExtentProvenance::{Declared, Inferred};
        use SymbolKind::{Data, Function};

        let image = sectioned_symbol_test_image(
            &[
                ("function", 0x100, 0x140, Declared, Function, Global, true),
                // A data object inside code never displaces the function.
                ("table", 0x120, 0x130, Declared, Data, Global, true),
                ("outer", 0x800, 0x840, Declared, Data, Global, true),
                ("inner", 0x810, 0x818, Declared, Data, Local, false),
                // Unsized data names only its own address.
                ("label", 0x880, 0x880, Declared, Data, Global, true),
                ("inside", 0x820, 0x820, Declared, Data, Global, true),
            ],
            &[
                (".text", 0x100, 0x200, true),
                (".data", 0x800, 0x900, false),
                // A malformed overlapping section loses to the innermost one.
                (".overlap", 0x7f0, 0x8f0, false),
            ],
        );
        let described = |address: u64| {
            let description = image.describe(ImageAddress::new(address));
            (
                description
                    .section
                    .map(|section| (section.name.to_string(), section.offset)),
                description.symbol.map(|symbol| {
                    (
                        symbol.name.to_string(),
                        symbol.kind,
                        symbol.offset,
                        symbol.provenance,
                    )
                }),
            )
        };

        let text = |offset| Some((".text".to_owned(), offset));
        let data = |offset| Some((".data".to_owned(), offset));
        let symbol = |name: &str, kind, offset, provenance| {
            Some((name.to_owned(), kind, offset, provenance))
        };

        assert_eq!(
            described(0x124),
            (text(0x24), symbol("function", Function, 0x24, Declared))
        );
        assert_eq!(described(0x150), (text(0x50), None));
        assert_eq!(
            described(0x814),
            (data(0x14), symbol("inner", Data, 4, Declared))
        );
        assert_eq!(
            described(0x818),
            (data(0x18), symbol("outer", Data, 0x18, Declared))
        );

        // Sized storage wins over an unsized label at the same address.
        assert_eq!(
            described(0x820),
            (data(0x20), symbol("outer", Data, 0x20, Declared))
        );
        assert_eq!(described(0x840), (data(0x40), None));
        assert_eq!(
            described(0x880),
            (data(0x80), symbol("label", Data, 0, Inferred))
        );
        assert_eq!(described(0x881), (data(0x81), None));
        assert_eq!(described(0x7f8), (Some((".overlap".to_owned(), 8)), None));
        assert_eq!(described(0x900), (None, None));
    }
}
